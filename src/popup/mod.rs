//! The quick-settings popup.
//!
//! One window, anchored to a panel, containing every control: there is no
//! separate settings app, so clicking the Wi-Fi icon in the bar opens this
//! window and clicking Wi-Fi again slides to its list *inside the same window*.
//! Navigation is a `gtk::Stack` rather than a second surface, which is what
//! keeps this one layer-shell surface and therefore one keyboard grab.
//!
//! A consequence of the keyboard grab shapes the whole design: nothing here
//! opens a second toplevel. A new window would be visible but the compositor
//! would not give it keyboard focus while the popup holds the grab, so every
//! prompt and every confirmation is built inline, inside this surface.
//!
//! Update strategy: every control keeps its widgets and mutates them in place.
//! Lists are only rebuilt when a cheap fingerprint of their contents changes,
//! because tearing a list down on every state change is what makes panels
//! flicker and accumulate dead signal handlers.

mod apps;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use crate::config::QuickSettings as QsConfig;
use crate::dbus::Subscription;
use crate::layer;
use crate::service;
use crate::service::bluetooth::BluetoothDevice;
use crate::service::network::AccessPoint;
use crate::state::{self, State};
use crate::ui::{self, PageHeader, SliderRow, Tile};
use crate::{debug, info, warn};

/// Which page is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Root,
    Wifi,
    Bluetooth,
    Apps,
    Session,
    About,
    Notifications,
}

impl Page {
    fn stack_name(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Wifi => "wifi",
            Self::Bluetooth => "bluetooth",
            Self::Apps => "apps",
            Self::Session => "session",
            Self::About => "about",
            Self::Notifications => "notifications",
        }
    }
}

pub struct QuickSettings {
    window: RefCell<Option<gtk4::ApplicationWindow>>,
    app: gtk4::Application,
    cfg: QsConfig,
    page: Cell<Page>,
    visible: Cell<bool>,
    persist: Rc<State>,

    /// Distance from the anchored edge, in pixels, supplied by the application
    /// from the height of the panel the popup hangs off.
    anchor_offset: Cell<i32>,

    /// Whether losing keyboard focus closes the popup.
    close_on_blur: bool,

    /// Emits on every open/close.
    visible_emitter: crate::dbus::Emitter<bool>,

    stack: gtk4::Stack,
    root_page: gtk4::Box,
    grid: gtk4::Grid,
    battery_pill: gtk4::Label,
    volume_row: SliderRow,
    brightness_row: SliderRow,
    /// Colour-temperature row, shown only when a night-light back-end exists.
    temperature_row: SliderRow,
    outputs: OutputPicker,
    inputs: OutputPicker,
    tiles: RefCell<Vec<(String, Tile)>>,
    wifi: WifiPage,
    bluetooth: BtPage,
    launcher: apps::AppLauncher,
    session: Rc<SessionPage>,
    about: gtk4::Box,
    notifications_root: gtk4::Box,
    notifications_rows: gtk4::ListBox,
    notifications_scroll: gtk4::ScrolledWindow,
    _subs: RefCell<Vec<Subscription>>,
}

impl QuickSettings {
    pub fn new(app: &gtk4::Application, cfg: &QsConfig, close_on_blur: bool) -> Rc<Self> {
        let me = Rc::new(Self {
            window: RefCell::new(None),
            app: app.clone(),
            cfg: cfg.clone(),
            page: Cell::new(Page::Root),
            visible: Cell::new(false),
            persist: Rc::new(State::load()),
            anchor_offset: Cell::new(0),
            close_on_blur,
            visible_emitter: crate::dbus::Emitter::new(),
            stack: gtk4::Stack::new(),
            root_page: ui::vbox(10),
            grid: gtk4::Grid::new(),
            battery_pill: gtk4::Label::new(None),
            volume_row: SliderRow::new("audio-volume-medium-symbolic", |_| {}),
            brightness_row: SliderRow::new("display-brightness-symbolic", |_| {}),
            temperature_row: SliderRow::new("night-light-symbolic", |_| {}),
            outputs: OutputPicker::new(Kind::Sink),
            inputs: OutputPicker::new(Kind::Source),
            tiles: RefCell::new(Vec::new()),
            wifi: WifiPage::new(),
            bluetooth: BtPage::new(),
            launcher: apps::AppLauncher::new(),
            session: Rc::new(SessionPage::new()),
            about: about_page(),
            notifications_root: ui::vbox(0),
            notifications_rows: gtk4::ListBox::new(),
            notifications_scroll: gtk4::ScrolledWindow::new(),
            _subs: RefCell::new(Vec::new()),
        });

        me.stack.set_transition_duration(180);
        me.stack.set_hexpand(true);
        me.stack.set_vexpand(true);

        me.build_root_page();
        me.build_pages();
        me.wire_services();
        me
    }

    // -- window lifecycle ---------------------------------------------------

    /// The window is created on first use, so a session that never opens the
    /// popup never maps a surface.
    fn ensure_window(self: &Rc<Self>) {
        if self.window.borrow().is_some() {
            return;
        }
        let window = gtk4::ApplicationWindow::builder()
            .application(&self.app)
            .title("ra")
            .resizable(false)
            .decorated(false)
            .build();
        window.add_css_class("ra-popup");
        window.set_default_size(self.cfg.width as i32, -1);

        // Layer-shell configuration must happen before the surface is realised.
        layer::attach_popup(&window, self.cfg.anchor, false);
        self.set_margins(&window);

        let surface = ui::vbox(0);
        surface.add_css_class("ra-popup-surface");

        // The scroller wraps the stack, so a page taller than the screen still
        // scrolls. Only the stack goes in it - a widget can have one parent.
        //
        // `propagate_natural_height` is what lets the popup shrink to fit a
        // short page. Without it the window would take the largest height any
        // page ever needed, and a four-tile popup would sit on a slab of empty
        // background with the tiles floating at the top.
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
        // Capped so a long list scrolls instead of running off the screen. The
        // bound is the monitor, minus room for the surface's own margins.
        scroller.set_max_content_height(self.max_page_height());
        scroller.add_css_class("ra-scroller");
        scroller.set_child(Some(&self.stack));

        surface.append(&scroller);
        window.set_child(Some(&surface));

        window.connect_close_request(|win| {
            // The compositor can ask us to close (for example the output
            // vanished). Hide rather than destroy, so the next open is cheap.
            win.set_visible(false);
            glib::Propagation::Proceed
        });

        let weak = Rc::downgrade(self);
        let close_on_blur = self.close_on_blur;
        window.connect_notify_local(Some("is-active"), move |win, _| {
            if !close_on_blur {
                return;
            }
            let Some(me) = weak.upgrade() else { return };
            // Only close when we actually opened it and then lost focus.
            if !win.is_active() && me.visible.get() {
                me.hide();
            }
        });

        *self.window.borrow_mut() = Some(window);
    }

    /// The tallest a page may be before it starts scrolling.
    ///
    /// Derived from the monitor rather than hard-coded, so the popup fills a
    /// 4K screen without being told to and still fits a 768-pixel laptop.
    fn max_page_height(&self) -> i32 {
        const MARGIN: i32 = 48;
        const CEILING: i32 = 720;
        // GTK4 dropped the primary-monitor concept, so this takes the first
        // output the display reports - the same one the panels anchor to.
        let screen = gtk4::gdk::Display::default()
            .and_then(|d| d.monitors().item(0))
            .and_then(|m| m.downcast::<gtk4::gdk::Monitor>().ok())
            .map(|m| m.geometry().height())
            .unwrap_or(CEILING + MARGIN);
        (screen - MARGIN).clamp(240, CEILING)
    }

    fn set_margins(&self, window: &gtk4::ApplicationWindow) {
        // Size to the content, on every compositor. The scroller caps growth.
        window.set_default_size(self.cfg.width as i32, -1);
        if !layer::supported() {
            return;
        }
        let gap = self.cfg.gap as i32;
        let (edge, value) = if self.cfg.anchor.is_bottom() {
            (gtk4_layer_shell::Edge::Bottom, gap)
        } else {
            (gtk4_layer_shell::Edge::Top, self.anchor_offset.get() + gap)
        };
        gtk4_layer_shell::LayerShell::set_margin(window, edge, value);
    }

    /// Called by the application once the anchor panel's geometry is known.
    /// The popup window, once it has been built.
    ///
    /// Created lazily, so this is `None` until the popup has been opened at
    /// least once. `ra --screenshot` opens it first.
    pub fn window(&self) -> Option<gtk4::ApplicationWindow> {
        self.window.borrow().clone()
    }

    pub fn set_anchor_offset(&self, offset: i32) {
        self.anchor_offset.set(offset);
    }

    /// Apply the poll interval for values the kernel offers no signal for.
    /// Zero disables the fallback timer entirely.
    pub fn apply_poll_interval(&self, ms: u32) {
        service::brightness::get().set_poll_interval(ms);
    }

    pub fn open(self: &Rc<Self>) {
        if !self.cfg.enabled {
            return;
        }
        self.ensure_window();
        self.go(Page::Root);
        self.refresh();

        if let Some(window) = self.window.borrow().clone() {
            self.set_margins(&window);
            window.present();
            // Take the keyboard grab *after* presenting: asking for `Exclusive`
            // on a surface the compositor has not seen yet is a no-op.
            layer::attach_popup(&window, self.cfg.anchor, true);
        }
        self.visible.set(true);
        self.visible_emitter.emit(&true);
        service::audio::get().set_active(true);
        service::brightness::get().set_active(true);
        debug!("quick settings opened");
    }

    pub fn hide(&self) {
        if !self.visible.replace(false) {
            return;
        }
        self.visible_emitter.emit(&false);
        service::audio::get().set_active(false);
        service::brightness::get().set_active(false);
        service::bluetooth::get().set_discovering(false);
        if let Some(window) = self.window.borrow().clone() {
            layer::release_focus(&window);
            window.set_visible(false);
        }
    }

    /// Open the application grid directly, used by the launcher module.
    pub fn open_apps(self: &Rc<Self>) {
        self.open();
        self.go(Page::Apps);
    }

    pub fn toggle(self: &Rc<Self>) {
        if self.visible.get() {
            self.hide();
        } else {
            self.open();
        }
    }

    /// Notified whenever the popup opens or closes, so indicators can keep
    /// their pressed state right even when the popup is dismissed by a click
    /// somewhere else.
    pub fn connect_visible(&self, f: impl Fn(bool) + 'static) -> Subscription {
        self.visible_emitter.subscribe(move |visible| f(*visible))
    }

    fn go(self: &Rc<Self>, page: Page) {
        self.page.set(page);
        self.stack.set_visible_child_name(page.stack_name());
        match page {
            Page::Wifi => {
                self.wifi.refresh();
                service::network::get().request_scan();
            }
            Page::Bluetooth => {
                self.bluetooth.refresh();
                service::bluetooth::get().set_discovering(true);
            }
            Page::Apps => self.launcher.focus_search(),
            Page::Notifications => {
                self.refresh_notifications();
            }
            _ => service::bluetooth::get().set_discovering(false),
        }
    }

    // -- state --------------------------------------------------------------

    fn refresh(self: &Rc<Self>) {
        let net = service::network::get();
        let bt = service::bluetooth::get();
        let audio = service::audio::get();
        let bright = service::brightness::get();
        let power = service::power::get();

        let battery = power.battery();
        self.battery_pill.set_text(&battery.label());

        for (name, tile) in self.tiles.borrow().iter() {
            match name.as_str() {
                "network" => {
                    let s = net.state();
                    tile.set_icon(s.icon());
                    tile.set_subtitle(&s.label());
                    tile.set_on(s.wifi_present && s.wifi_enabled);
                }
                "bluetooth" => {
                    let s = bt.state();
                    tile.set_icon(s.icon());
                    tile.set_subtitle(&s.label());
                    tile.set_on(s.powered);
                }
                "power-profile" => {
                    let s = power.profile();
                    tile.set_icon(s.icon());
                    tile.set_subtitle(&s.label());
                    tile.set_sensitive(s.profiles.len() > 1);
                }
                "microphone" => {
                    let s = audio.state().source_muted;
                    tile.set_icon(if s {
                        "microphone-sensitivity-muted-symbolic"
                    } else {
                        "audio-input-microphone-symbolic"
                    });
                    tile.set_on(s);
                    tile.set_subtitle(if s { "Muted" } else { "Live" });
                }
                // The four tiles below are stateful too, and leaving them out is
                // how the popup ends up contradicting itself: Wi-Fi shows a
                // connected network while Airplane Mode claims to be on,
                // because Airplane Mode was drawn once at build time, before
                // NetworkManager had answered, and never redrawn. Anything that
                // shows state has to be re-derived here, not only in its
                // construction arm.
                "airplane" => {
                    // Airplane mode means both radios are down. It is not the
                    // same as being offline: Wi-Fi on with Bluetooth off is a
                    // perfectly normal state, and calling that "on" would make
                    // the tile lie about a connection that is working.
                    let wifi_on = net.state().wifi_enabled;
                    let bluetooth_on = bt.state().powered;
                    let on = !wifi_on && !bluetooth_on;
                    tile.set_icon(if on {
                        "network-offline-symbolic"
                    } else {
                        "network-wireless-symbolic"
                    });
                    tile.set_on(on);
                    tile.set_subtitle(if on { "On" } else { "Off" });
                }
                "night-light" => {
                    let s = service::nightlight::get(None, None);
                    let state = s.state();
                    tile.set_sensitive(state.available);
                    tile.set_on(state.available && state.enabled);
                    let subtitle = if !state.available {
                        "Unavailable".to_string()
                    } else if state.enabled {
                        format!("{}K", state.temperature)
                    } else {
                        "Off".to_string()
                    };
                    tile.set_subtitle(&subtitle);
                }
                "dark-style" => {
                    let on = self.persist.get_bool(state::KEY_DARK, false);
                    tile.set_icon(if on {
                        "weather-night-symbolic"
                    } else {
                        "weather-clear-day-symbolic"
                    });
                    tile.set_on(on);
                    tile.set_subtitle(if on { "On" } else { "Off" });
                }
                "do-not-disturb" => {
                    let on = self.persist.get_bool(state::KEY_DND, false);
                    tile.set_on(on);
                    tile.set_subtitle(if on { "On" } else { "Off" });
                }
                _ => {}
            }
        }

        let audio = audio.state();
        self.volume_row.set_icon(audio.icon());
        self.volume_row.set_value(audio.volume);
        self.volume_row.set_visible_row(self.cfg.volume && audio.available);
        self.outputs.set_visible(self.cfg.volume && audio.available && audio.sinks.len() > 1);
        self.outputs.refresh(&audio.sinks);
        if self.cfg.microphone {
            self.inputs.set_visible(audio.available && audio.sources.len() > 1);
            self.inputs.refresh(&audio.sources);
        }

        let bright = bright.state();
        self.brightness_row.set_icon(bright.icon());
        self.brightness_row.set_value(bright.value);
        self.brightness_row.set_visible_row(self.cfg.brightness && bright.available);

        let night = service::nightlight::get(None, None);
        self.temperature_row.set_visible_row(night.state().available);
        if night.state().available {
            let kelvin = night.state().temperature;
            self.temperature_row.set_value(((6500 - kelvin.min(6500)) * 100 / 5500) as u8);
        }
    }

    fn build_root_page(self: &Rc<Self>) {
        let root = &self.root_page;

        // Header: battery pill on the left, round actions on the right.
        let header = ui::hbox(8);
        header.add_css_class("ra-popup-header");

        let pill = ui::button(&["ra-user-pill"]);
        pill.set_child(Some(&self.battery_pill));
        pill.set_hexpand(false);
        ui::tooltip(&pill, "Battery, lock, suspend and power off");
        pill.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(me) = weak.upgrade() {
                    me.go(Page::Session);
                }
            }
        });
        header.append(&pill);

        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header.append(&spacer);

        for (icon_name, tip, target) in [
            ("view-app-grid-symbolic", "Applications", Some(Page::Apps)),
            ("preferences-system-symbolic", "Desktop settings", None::<Page>),
            ("system-shutdown-symbolic", "Session", Some(Page::Session)),
            ("applications-system-symbolic", "About ra", Some(Page::About)),
        ] {
            let b = ui::button(&["ra-round-button"]);
            crate::icons::set_on_button(&b, icon_name, 16);
            ui::tooltip(&b, tip);
            match target {
                Some(page) => {
                    let weak = Rc::downgrade(self);
                    b.connect_clicked(move |_| {
                        if let Some(me) = weak.upgrade() {
                            me.go(page);
                        }
                    });
                }
                None => {
                    b.connect_clicked(|_| {
                        // Deliberately not xfce4-settings-manager: that is the
                        // panel being replaced. Open GTK Settings so the user
                        // still has somewhere to change theme, fonts and input.
                        gtk4::UriLauncher::new("gnome-control-center").launch(
                            None::<&gtk4::Window>,
                            gtk4::gio::Cancellable::NONE,
                            |_| {},
                        );
                    });
                }
            }
            header.append(&b);
        }
        root.append(&header);

        // The notifications row lives below the tiles. It is visible whenever
        // the popup is open and the daemon is either off (nothing to show) or
        // running (deferred messages are shown once DND turns off). It is never
        // rebuilt per change: the list rows update in place, which is what
        // keeps this fast on a slow compositor.
        self.notifications_rows.add_css_class("ra-notifications-list");
        self.notifications_rows.set_selection_mode(gtk4::SelectionMode::None);
        self.notifications_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        self.notifications_scroll.set_propagate_natural_height(true);
        self.notifications_scroll.set_max_content_height(180);
        self.notifications_scroll.set_child(Some(&self.notifications_rows));
        if self.cfg.notifications {
            self.notifications_root.append(&self.notifications_scroll);
            self.notifications_root.set_size_request(-1, 150);
            root.append(&self.notifications_root);
        }

        root.append(&self.volume_row.root);
        root.append(&self.brightness_row.root);
        root.append(self.outputs.widget());
        if self.cfg.microphone {
            root.append(self.inputs.widget());
        }
        root.append(&self.temperature_row.root);

        self.grid.set_column_spacing(8);
        self.grid.set_row_spacing(8);
        self.grid.set_column_homogeneous(true);
        self.grid.set_row_homogeneous(true);
        root.append(&self.grid);

        self.build_tiles();

        // The root page goes in first so it is what `set_visible_child_name`
        // finds on the very first frame.
        self.stack.add_named(&self.root_page, Some("root"));
    }

    fn build_tiles(self: &Rc<Self>) {
        let mut made: Vec<(String, Tile)> = Vec::new();
        for (index, name) in self.cfg.tiles.iter().enumerate() {
            // `microphone` gates its tile the same way `volume` and
            // `brightness` gate their rows, so the whole set stays switchable
            // from one place.
            if name == "microphone" && !self.cfg.microphone {
                continue;
            }
            let Some(tile) = self.make_tile(name) else {
                warn!(
                    "unknown quick-settings tile `{name}`; known tiles are {} - skipping it",
                    QuickSettings::known_tiles().join(", ")
                );
                continue;
            };
            self.grid.attach(&tile.root, (index % 2) as i32, (index / 2) as i32, 1, 1);
            made.push((name.clone(), tile));
        }
        *self.tiles.borrow_mut() = made;
    }

    /// Every tile this build can draw.
    ///
    /// Kept alongside the `make_tile` match it describes. The two can drift,
    /// which is why `make_tile` returns `None` for anything it does not
    /// recognise rather than drawing an empty box - but this list is what the
    /// configuration defaults are checked against, so a name added here without
    /// a matching match arm is caught by
    /// `config::tests::every_default_tile_is_implemented` failing in the
    /// honest direction: the test asks the popup what it can draw.
    pub fn known_tiles() -> &'static [&'static str] {
        &[
            "airplane",
            "bluetooth",
            "dark-style",
            "do-not-disturb",
            "microphone",
            "network",
            "night-light",
            "power-profile",
            "session",
        ]
    }

    fn make_tile(self: &Rc<Self>, name: &str) -> Option<Tile> {
        let weak = Rc::downgrade(self);
        let drill = matches!(name, "network" | "bluetooth" | "session");
        let tile = Tile::new(&["ra-tile"], drill);

        match name {
            "network" => {
                tile.set_title("Wi-Fi");
                tile.set_subtitle("...");
                tile.root.connect_clicked(move |_| {
                    if let Some(me) = weak.upgrade() {
                        me.go(Page::Wifi);
                    }
                });
                ui::tooltip(&tile.root, "Wi-Fi networks and settings");
            }
            "bluetooth" => {
                tile.set_title("Bluetooth");
                tile.set_subtitle("...");
                tile.root.connect_clicked(move |_| {
                    if let Some(me) = weak.upgrade() {
                        me.go(Page::Bluetooth);
                    }
                });
                ui::tooltip(&tile.root, "Bluetooth devices and settings");
            }
            "power-profile" => {
                tile.set_title("Power Mode");
                tile.set_subtitle("...");
                tile.root.connect_clicked({
                    let power = service::power::get();
                    move |_| {
                        let profiles = power.profile().profiles.clone();
                        if profiles.len() <= 1 {
                            return;
                        }
                        // Cycling is faster than a sub-page for three states
                        // and needs no extra surface.
                        let active = power.profile().active.clone();
                        let next = profiles
                            .iter()
                            .find(|p| active.as_deref() != Some(p.as_str()))
                            .cloned()
                            .unwrap_or_else(|| profiles[0].clone());
                        power.set_profile_by_name(&next);
                    }
                });
                ui::tooltip(&tile.root, "Switch power profile");
            }
            "night-light" => {
                tile.set_title("Night Light");
                tile.set_icon("night-light-symbolic");
                let night = service::nightlight::get(None, None);
                tile.set_on(night.state().enabled);
                tile.set_subtitle(&night.state().temperature.to_string());
                tile.set_sensitive(night.state().available);
                let available = night.state().available;
                ui::tooltip(
                    &tile.root,
                    if available {
                        "Shift the screen towards red at night"
                    } else {
                        "No night-light back-end found. Install gamut, gammastep or redshift."
                    },
                );
                tile.root.connect_clicked(move |_| night.toggle());
            }
            "dark-style" => {
                tile.set_title("Dark Style");
                tile.set_icon("weather-night-symbolic");
                let dark = self.persist.get_bool(state::KEY_DARK, false);
                tile.set_on(dark);
                tile.set_subtitle(if dark { "On" } else { "Off" });
                tile.root.connect_clicked({
                    let weak = Rc::downgrade(self);
                    move |_| {
                        let Some(me) = weak.upgrade() else { return };
                        let next = !me.persist.get_bool(state::KEY_DARK, false);
                        me.persist.set_bool(state::KEY_DARK, next);
                        crate::module::basic::set_prefer_dark(next);
                        me.refresh();
                    }
                });
            }
            "do-not-disturb" => {
                tile.set_title("Do Not Disturb");
                tile.set_icon("notifications-disabled-symbolic");
                let on = self.persist.get_bool(state::KEY_DND, false);
                tile.set_on(on);
                tile.set_subtitle(if on { "On" } else { "Off" });
                tile.root.connect_clicked({
                    let weak = Rc::downgrade(self);
                    move |_| {
                        let Some(me) = weak.upgrade() else { return };
                        let next = !me.persist.get_bool(state::KEY_DND, false);
                        me.persist.set_bool(state::KEY_DND, next);
                        crate::service::notification::get().set_dnd(next);
                        me.refresh();
                    }
                });
                ui::tooltip(
                    &tile.root,
                    "Silence notifications. Applies to notifications ra presents; suppressing \
                     other applications' pop-ups requires owning the notification daemon.",
                );
            }
            "airplane" => {
                tile.set_title("Airplane Mode");
                tile.set_icon("network-offline-symbolic");
                let net = service::network::get();
                let bt = service::bluetooth::get();
                let on = !net.state().wifi_enabled && !bt.state().powered;
                tile.set_on(on);
                tile.set_subtitle(if on { "On" } else { "Off" });
                tile.root.connect_clicked(move |_| {
                    let turning_on = net.state().wifi_enabled || bt.state().powered;
                    net.set_wifi_enabled(!turning_on);
                    bt.set_powered(!turning_on);
                });
            }
            "microphone" => {
                tile.set_title("Microphone");
                let audio = service::audio::get();
                let state = audio.state();
                tile.set_icon(if state.source_muted {
                    "microphone-sensitivity-muted-symbolic"
                } else {
                    "audio-input-microphone-symbolic"
                });
                tile.set_on(state.source_muted);
                tile.set_subtitle(if state.source_muted { "Muted" } else { "Live" });
                let toggle = audio.clone();
                tile.root.connect_clicked(move |_| {
                    toggle.toggle_source_mute();
                });
                ui::tooltip(&tile.root, "Mute or unmute the default microphone");
            }
            "session" => {
                tile.set_title("Session");
                tile.set_icon("system-log-out-symbolic");
                tile.set_subtitle("Lock, suspend, power off");
                tile.root.connect_clicked(move |_| {
                    if let Some(me) = weak.upgrade() {
                        me.go(Page::Session);
                    }
                });
            }
            _ => return None,
        }
        Some(tile)
    }

    fn build_pages(self: &Rc<Self>) {
        // The session page needs a weak handle on itself for its tile handlers
        // and for the logind permission stream.
        self.session.wire();

        let pages: [(Page, gtk4::Widget); 6] = [
            (Page::Wifi, self.wifi.root.clone().upcast()),
            (Page::Bluetooth, self.bluetooth.root.clone().upcast()),
            (Page::Apps, self.launcher.root.clone().upcast()),
            (Page::Session, self.session.root.clone().upcast()),
            (Page::About, self.about.clone().upcast()),
            (Page::Notifications, self.notifications_root.clone().upcast()),
        ];
        for (page, child) in pages {
            self.stack.add_named(&child, Some(page.stack_name()));
        }

        for header in [&self.wifi.header, &self.bluetooth.header, self.launcher.header()] {
            header.back.connect_clicked({
                let weak = Rc::downgrade(self);
                move |_| {
                    if let Some(me) = weak.upgrade() {
                        me.go(Page::Root);
                    }
                }
            });
        }

        self.stack.set_visible_child_name("root");
    }

    fn refresh_notifications(self: &Rc<Self>) {
        if !self.cfg.notifications {
            return;
        }
        let daemon = crate::service::notification::get();
        let notifications = daemon.visible();
        let _deferred = daemon.deferred_count();

        while let Some(child) = self.notifications_rows.first_child() {
            self.notifications_rows.remove(&child.upcast::<gtk4::Widget>());
        }

        for notification in notifications {
            let row = gtk4::ListBoxRow::new();
            row.add_css_class("ra-notification-row");
            row.set_selectable(false);
            row.set_activatable(false);

            let layout = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            layout.set_margin_start(12);
            layout.set_margin_end(12);
            layout.set_margin_top(6);
            layout.set_margin_bottom(6);

            let app_icon = crate::icons::resolve(&notification.app_icon);
            let icon = crate::icons::image(&app_icon, 22);
            icon.add_css_class("ra-notification-icon");
            if icon.parent().is_none() { layout.append(&icon); }

            let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
            text.set_hexpand(true);

            let summary = gtk4::Label::builder()
                .label(&notification.summary)
                .xalign(0.0)
                .build();
            summary.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            summary.add_css_class("ra-notification-summary");
            text.append(&summary);

            let subtitle = gtk4::Label::builder()
                .label(&format!("{} - {} · {}", notification.app_name,
                    notification.age_label(), notification.body.chars().take(48).collect::<String>()))
                .xalign(0.0)
                .build();
            subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            subtitle.add_css_class("ra-notification-subtitle");
            text.append(&subtitle);

            layout.append(&text);

            let close = gtk4::Button::builder()
                .icon_name("window-close-symbolic")
                .css_classes(["ra-round-button", "ra-notification-close"])
                .build();
            let close_id = notification.id;
            close.connect_clicked({
                let daemon = daemon.clone();
                move |_| {
                    daemon.close(close_id);
                }
            });
            if close.parent().is_none() { layout.append(&close); }

            if layout.parent().is_none() { row.set_child(Some(&layout)); }
            if row.parent().is_none() { self.notifications_rows.append(&row); }
        }

        // Deferred counter shown in the subtitle row of the DND tile only,
        // not duplicated in the notifications list itself.
        self.battery_pill.set_visible(self.visible.get() && self.page.get() == Page::Notifications);
    }

    fn wire_services(self: &Rc<Self>) {
        if self.cfg.notifications && !self.cfg.notification_daemon {
            // Showing the list requires owning the daemon; if the daemon is
            // off but the list is on, warn rather than showing an empty list
            // that looks like nothing is wrong.
            warn!("quick_settings.notifications is true but notification_daemon is false;                     notifications will not appear until daemon is enabled");
        }
        let mut subs = Vec::new();

        // Repaint only while the popup is on screen. Closing it means the
        // service updates keep arriving, but nothing is drawn, which is the
        // whole point of making them signal-driven.
        fn repaint<T>(weak: &std::rc::Weak<QuickSettings>) -> impl Fn(&T) + 'static {
            let weak = weak.clone();
            move |_| {
                if let Some(me) = weak.upgrade() {
                    if me.visible.get() {
                        me.refresh();
                    }
                }
            }
        }

        let weak = Rc::downgrade(self);
        subs.push(service::network::get().on_change(repaint(&weak)));
        subs.push(service::bluetooth::get().on_change(repaint(&weak)));
        subs.push(service::audio::get().on_change(repaint(&weak)));
        subs.push(service::power::get().on_battery_change(repaint(&weak)));
        subs.push(service::power::get().on_profile_change(repaint(&weak)));
        subs.push(service::brightness::get().on_change(repaint(&weak)));
        subs.push(service::nightlight::get(None, None).on_change(repaint(&weak)));



        // Slider drags emit continuously; coalesce onto an idle so a drag
        // cannot queue hundreds of calls behind the pointer.
        let audio = service::audio::get();
        self.volume_row.slider.connect_value_changed(move |s| {
            let v = s.value().round().clamp(0.0, 100.0) as u8;
            let audio = audio.clone();
            glib::idle_add_local_once(move || audio.set_volume(v));
        });
        let bright = service::brightness::get();
        self.brightness_row.slider.connect_value_changed(move |s| {
            let v = s.value().round().clamp(0.0, 100.0) as u8;
            let bright = bright.clone();
            glib::idle_add_local_once(move || bright.set(v));
        });

        // The temperature slider is inverted: pushing it right makes the screen
        // cooler, matching the intuition of a physical slider.
        let night = service::nightlight::get(None, None);
        self.temperature_row.slider.connect_value_changed(move |s| {
            let fraction = (s.value().round().clamp(0.0, 100.0) / 100.0).min(1.0);
            let kelvin = 6500 - (fraction * 5500.0).round() as u32;
            let night = night.clone();
            glib::idle_add_local_once(move || night.set_temperature(kelvin.clamp(1000, 6500)));
        });

        *self._subs.borrow_mut() = subs;
    }
}

impl Drop for QuickSettings {
    fn drop(&mut self) {
        // Never leave the adapter scanning after the popup goes away.
        service::bluetooth::get().set_discovering(false);
    }
}

// ---------------------------------------------------------------------------
// Device pickers
// ---------------------------------------------------------------------------

/// A list of audio devices, revealed under a slider row.
struct OutputPicker {
    scroller: gtk4::ScrolledWindow,
    rows: gtk4::ListBox,
    /// Fingerprint of what is currently rendered.
    shown: RefCell<Vec<(u32, String, bool)>>,
    /// Sinks or sources; the only difference is which `set-default` is called.
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Sink,
    Source,
}

impl OutputPicker {
    fn new(kind: Kind) -> Self {
        let rows = gtk4::ListBox::new();
        rows.set_selection_mode(gtk4::SelectionMode::None);
        rows.add_css_class("ra-list");

        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::External);
        scroller.set_propagate_natural_height(true);
        scroller.set_max_content_height(126);
        scroller.set_propagate_natural_width(true);
        scroller.add_css_class("ra-scroller");
        scroller.set_child(Some(&rows));
        scroller.set_visible(false);

        Self { scroller, rows, shown: RefCell::new(Vec::new()), kind }
    }

    fn widget(&self) -> &gtk4::ScrolledWindow {
        &self.scroller
    }

    fn set_visible(&self, visible: bool) {
        self.scroller.set_visible(visible);
    }

    fn refresh(&self, devices: &[service::audio::AudioDevice]) {
        let wanted: Vec<(u32, String, bool)> =
            devices.iter().map(|d| (d.id, d.description.clone(), d.is_default)).collect();
        if *self.shown.borrow() == wanted {
            return;
        }
        *self.shown.borrow_mut() = wanted.clone();

        clear_list(&self.rows);
        for device in devices {
            let row = gtk4::ListBoxRow::new();
            row.set_selectable(false);
            if device.is_default {
                row.add_css_class("connected");
            }
            let label = gtk4::Label::builder()
                .label(&device.description)
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .build();
            label.set_hexpand(true);
            let wrap = ui::hbox(8);
            wrap.set_margin_top(3);
            wrap.set_margin_bottom(3);
            wrap.append(&label);
            if device.is_default {
                let check = ui::icon("object-select-symbolic", 14);
                check.add_css_class("ra-chevron");
                wrap.append(&check);
            }
            row.set_child(Some(&wrap));
            let id = device.id;
            let audio = service::audio::get();
            let kind = self.kind;
            row.connect_activate(move |_| match kind {
                Kind::Sink => audio.set_default_sink(id),
                Kind::Source => audio.set_default_source(id),
            });
            self.rows.append(&row);
        }
    }
}

// ---------------------------------------------------------------------------
// Wi-Fi page
// ---------------------------------------------------------------------------

struct WifiPage {
    root: gtk4::Box,
    header: PageHeader,
    list: gtk4::ScrolledWindow,
    rows: gtk4::ListBox,
    status: gtk4::Label,
    shown: RefCell<Vec<String>>,
}

impl WifiPage {
    fn new() -> Self {
        let root = ui::vbox(8);
        root.set_size_request(-1, 290);

        let header = PageHeader::new();
        header.set_title("Wi-Fi");
        root.append(&header.root);

        let status = gtk4::Label::new(None);
        status.add_css_class("ra-empty");
        root.append(&status);

        let rows = gtk4::ListBox::new();
        rows.set_selection_mode(gtk4::SelectionMode::None);
        rows.add_css_class("ra-list");
        let list = gtk4::ScrolledWindow::new();
        list.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        list.set_propagate_natural_height(true);
        list.set_max_content_height(268);
        list.add_css_class("ra-scroller");
        list.set_child(Some(&rows));
        root.append(&list);

        Self { root, header, list, rows, status, shown: RefCell::new(Vec::new()) }
    }

    fn refresh(&self) {
        let net = service::network::get();
        {
            let state = net.state();
            if !state.available {
                return self.set_status("NetworkManager is not running.");
            }
            if !state.wifi_present {
                return self.set_status("This machine has no Wi-Fi adapter.");
            }
            if !state.wifi_enabled {
                return self.set_status("Wi-Fi is switched off");
            }
        }
        self.status.set_visible(false);
        self.list.set_visible(true);

        let weak = self.rows.downgrade();
        let shown = self.shown.clone();
        let net = net.clone();
        glib::spawn_future_local(async move {
            let aps = net.list_access_points().await;
            let Some(rows) = weak.upgrade() else { return };
            if rows.parent().is_none() {
                return;
            }
            render_aps(&rows, &shown, aps);
        });
    }

    fn set_status(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(true);
        self.list.set_visible(false);
    }
}

fn render_aps(rows: &gtk4::ListBox, shown: &RefCell<Vec<String>>, aps: Vec<AccessPoint>) {
    let fingerprint: Vec<String> = aps
        .iter()
        .map(|a| format!("{}/{}/{}/{}", a.ssid, a.strength, a.secure, a.connected))
        .collect();
    if *shown.borrow() == fingerprint {
        return;
    }
    *shown.borrow_mut() = fingerprint;

    clear_list(rows);
    if aps.is_empty() {
        rows.append(&message_row("No networks visible"));
        return;
    }
    for ap in aps {
        let row = gtk4::ListBoxRow::new();
        row.set_selectable(false);
        if ap.connected {
            row.add_css_class("connected");
        }
        let wrap = ui::hbox(10);
        wrap.set_margin_top(3);
        wrap.set_margin_bottom(3);
        wrap.append(&ui::icon(strength_icon(ap.strength), 18));

        let name = gtk4::Label::builder()
            .label(&ap.ssid)
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        name.set_hexpand(true);
        if ap.connected {
            name.add_css_class("ra-tile-title");
        }
        wrap.append(&name);

        if ap.secure {
            let lock = ui::icon("system-lock-symbolic", 13);
            lock.add_css_class("ra-chevron");
            wrap.append(&lock);
        }
        if ap.connected {
            let check = ui::icon("object-select-symbolic", 14);
            check.add_css_class("ra-chevron");
            wrap.append(&check);
        }

        row.set_child(Some(&wrap));
        row.set_tooltip_text(Some(&ap_tooltip(&ap)));
        let target = ap;
        let list = rows.clone();
        row.connect_activate(move |r| {
            if target.connected || !target.secure || target.saved {
                join(&target);
            } else {
                add_passphrase_row(&list, r, &target);
            }
        });
        rows.append(&row);
    }
}

fn ap_tooltip(ap: &AccessPoint) -> String {
    let mut s = format!("{}\nSignal {}%", ap.ssid, ap.strength);
    if ap.connected {
        s.push_str("\nConnected — activate to disconnect");
    } else if ap.secure && ap.saved {
        s.push_str("\nSecured, saved");
    } else if ap.secure {
        s.push_str("\nSecured — a passphrase will be requested");
    } else {
        s.push_str("\nOpen");
    }
    s
}

/// Join a network.
///
/// The passphrase prompt is *inline*, not a modal dialog, and that is a
/// deliberate consequence of how a layer-shell surface works: while the popup
/// holds the keyboard grab, a newly created toplevel would be visible but
/// would not be given keyboard focus by the compositor. Anything that needs
/// typing therefore has to live inside the popup surface itself.
fn join(ap: &AccessPoint) {
    let net = service::network::get();
    if ap.connected {
        net.disconnect();
    } else if !ap.secure || ap.saved {
        net.connect(ap, None);
    }
}

/// Insert a passphrase row directly beneath `anchor`.
fn add_passphrase_row(list: &gtk4::ListBox, anchor: &gtk4::ListBoxRow, ap: &AccessPoint) {
    let entry = gtk4::PasswordEntry::new();
    entry.set_placeholder_text(Some("Passphrase or pre-shared key"));
    entry.add_css_class("ra-entry");
    entry.set_hexpand(true);
    entry.set_valign(gtk4::Align::Center);

    let submit_button = ui::button(&["ra-round-button"]);
    crate::icons::set_on_button(&submit_button, "go-next-symbolic", 16);
    ui::tooltip(&submit_button, "Connect");
    let cancel_button = ui::button(&["ra-round-button"]);
    crate::icons::set_on_button(&cancel_button, "window-close-symbolic", 16);
    ui::tooltip(&cancel_button, "Cancel");

    let row = ui::hbox(8);
    row.set_margin_top(2);
    row.set_margin_bottom(2);
    row.append(&entry);
    row.append(&cancel_button);
    row.append(&submit_button);

    let container = gtk4::ListBoxRow::new();
    container.set_selectable(false);
    container.add_css_class("ra-passphrase");
    container.set_child(Some(&row));

    // `insert` needs an index; compute it once, up front.
    list.insert(&container, anchor.index().max(0) + 1);

    let remove = {
        let list = list.clone();
        let container = container.clone();
        move || list.remove(&container)
    };

    let target = ap.clone();
    let submit = {
        let remove = remove.clone();
        move |value: &str| {
            if value.is_empty() {
                crate::warn!("empty passphrase; not connecting");
            } else {
                service::network::get().connect(&target, Some(value));
            }
            remove();
        }
    };

    let for_button = submit.clone();
    let button_entry = entry.clone();
    submit_button.connect_clicked(move |_| for_button(button_entry.text().as_str()));

    // Enter submits, which is what a user expects from a one-line prompt.
    entry.connect_activate({
        let on_activate = submit.clone();
        move |e| on_activate(e.text().as_str())
    });

    cancel_button.connect_clicked(move |_| remove());
}

fn strength_icon(strength: u8) -> &'static str {
    match strength {
        80..=100 => "network-wireless-signal-excellent-symbolic",
        55..=79 => "network-wireless-signal-good-symbolic",
        30..=54 => "network-wireless-signal-ok-symbolic",
        _ => "network-wireless-signal-weak-symbolic",
    }
}

// ---------------------------------------------------------------------------
// Bluetooth page
// ---------------------------------------------------------------------------

struct BtPage {
    root: gtk4::Box,
    header: PageHeader,
    list: gtk4::ScrolledWindow,
    rows: gtk4::ListBox,
    status: gtk4::Label,
    shown: RefCell<Vec<String>>,
}

impl BtPage {
    fn new() -> Self {
        let root = ui::vbox(8);
        root.set_size_request(-1, 290);

        let header = PageHeader::new();
        header.set_title("Bluetooth");
        root.append(&header.root);

        let status = gtk4::Label::new(None);
        status.add_css_class("ra-empty");
        root.append(&status);

        let rows = gtk4::ListBox::new();
        rows.set_selection_mode(gtk4::SelectionMode::None);
        rows.add_css_class("ra-list");
        let list = gtk4::ScrolledWindow::new();
        list.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        list.set_propagate_natural_height(true);
        list.set_max_content_height(268);
        list.add_css_class("ra-scroller");
        list.set_child(Some(&rows));
        root.append(&list);

        Self { root, header, list, rows, status, shown: RefCell::new(Vec::new()) }
    }

    fn refresh(&self) {
        let bt = service::bluetooth::get();
        let state = bt.state();
        if !state.available {
            return self.set_status("bluetoothd is not running.");
        }
        if !state.present {
            return self.set_status("No Bluetooth adapter found.");
        }
        if !state.powered {
            return self.set_status("Bluetooth is switched off");
        }
        self.status.set_visible(false);
        self.list.set_visible(true);

        let weak = self.rows.downgrade();
        let shown = self.shown.clone();
        let bt = bt.clone();
        glib::spawn_future_local(async move {
            let devices = bt.list_devices().await;
            let Some(rows) = weak.upgrade() else { return };
            if rows.parent().is_none() {
                return;
            }
            render_devices(&rows, &shown, devices);
        });
    }

    fn set_status(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(true);
        self.list.set_visible(false);
    }
}

fn render_devices(rows: &gtk4::ListBox, shown: &RefCell<Vec<String>>, devices: Vec<BluetoothDevice>) {
    let fingerprint: Vec<String> =
        devices.iter().map(|d| format!("{}/{}/{}", d.address, d.name, d.connected)).collect();
    if *shown.borrow() == fingerprint {
        return;
    }
    *shown.borrow_mut() = fingerprint;

    clear_list(rows);
    if devices.is_empty() {
        rows.append(&message_row("No paired devices yet. Pair one in Bluetooth settings."));
        return;
    }
    for device in devices {
        let row = gtk4::ListBoxRow::new();
        row.set_selectable(false);
        if device.connected {
            row.add_css_class("connected");
        }
        let wrap = ui::hbox(10);
        wrap.set_margin_top(3);
        wrap.set_margin_bottom(3);
        wrap.append(&ui::icon(device_icon(&device.icon), 18));

        let name = gtk4::Label::builder()
            .label(&device.name)
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        name.set_hexpand(true);
        wrap.append(&name);

        if let Some(battery) = device.battery {
            let label = gtk4::Label::new(Some(&format!("{battery}%")));
            label.add_css_class("ra-tile-subtitle");
            wrap.append(&label);
        }

        let action = gtk4::Button::from_icon_name(if device.connected {
            "media-playback-stop-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
        action.add_css_class("flat");
        let path = device.path.clone();
        let connected = device.connected;
        action.connect_clicked(move |_| {
            let bt = service::bluetooth::get();
            if connected {
                bt.disconnect_device(&path);
            } else {
                bt.connect_device(&path);
            }
        });
        wrap.append(&action);

        row.set_child(Some(&wrap));
        row.set_tooltip_text(Some(&device.address));
        rows.append(&row);
    }
}

fn device_icon(bluez_icon: &str) -> &'static str {
    match bluez_icon {
        "audio-headset" | "audio-headphones" | "audio-card" => "audio-headphones-symbolic",
        "phone" => "phone-symbolic",
        "input-keyboard" => "input-keyboard-symbolic",
        "input-mouse" => "input-mouse-symbolic",
        "camera-video" => "camera-video-symbolic",
        "printer" => "printer-symbolic",
        _ => "bluetooth-symbolic",
    }
}

// ---------------------------------------------------------------------------
// Session and About
// ---------------------------------------------------------------------------

/// The session page: lock, suspend, restart, shut down, log out.
///
/// Owns its tiles so it can re-gate them when logind's idea of what is
/// permitted changes, and builds its confirmation strip inline. Both of those
/// are deliberate: a modal toplevel cannot take keyboard focus while the
/// layer-shell popup holds the grab, so a dialog would appear unresponsive on
/// exactly the screen the user is trying to leave.
struct SessionPage {
    root: gtk4::Box,
    header: PageHeader,
    grid: gtk4::Grid,
    /// Shown in place of the grid while a destructive action is pending.
    confirm: gtk4::Box,
    /// One tile per action, so each can be enabled or disabled individually.
    tiles: Vec<(SessionAction, Tile)>,
}

impl SessionPage {
    fn new() -> Self {
        let root = ui::vbox(8);
        root.set_size_request(-1, 200);

        let header = PageHeader::new();
        header.set_title("Session");
        root.append(&header.root);

        let grid = gtk4::Grid::new();
        grid.set_column_spacing(8);
        grid.set_row_spacing(8);
        grid.set_column_homogeneous(true);
        grid.set_row_homogeneous(true);

        let actions: [(SessionAction, &str, &str); 6] = [
            (SessionAction::Lock, "system-lock-screen-symbolic", "Lock"),
            (SessionAction::Suspend, "media-playback-pause-symbolic", "Suspend"),
            (SessionAction::Screensaver, "preferences-desktop-screensaver-symbolic", "Screensaver"),
            (SessionAction::Reboot, "system-reboot-symbolic", "Restart"),
            (SessionAction::PowerOff, "system-shutdown-symbolic", "Shut Down"),
            (SessionAction::Logout, "system-log-out-symbolic", "Log Out"),
        ];

        let mut tiles = Vec::with_capacity(actions.len());
        for (index, (action, icon_name, label)) in actions.into_iter().enumerate() {
            let tile = Tile::new(&["ra-tile"], false);
            tile.set_icon(icon_name);
            tile.set_title(label);
            tile.set_subtitle("");
            grid.attach(&tile.root, (index % 3) as i32, (index / 3) as i32, 1, 1);
            tiles.push((action, tile));
        }
        root.append(&grid);

        let confirm = ui::hbox(8);
        confirm.set_valign(gtk4::Align::Center);
        confirm.set_visible(false);
        root.append(&confirm);

        Self { root, header, grid, confirm, tiles }
    }

    /// Wire the tiles up. Every handler holds a `Weak`, so the page does not
    /// keep itself alive through the widgets it owns.
    fn wire(self: &Rc<Self>) {
        let buttons: Vec<(SessionAction, gtk4::Button)> =
            self.tiles.iter().map(|(action, tile)| (*action, tile.root.clone())).collect();
        for (action, button) in buttons {
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(page) = weak.upgrade() {
                    page.on_clicked(action);
                }
            });
        }

        let weak = Rc::downgrade(self);
        self.header.back.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                page.cancel();
            }
        });

        let power = service::power::get();
        self.apply_permissions(power.actions());
        let weak = Rc::downgrade(self);
        power.on_actions_change(move |actions| {
            if let Some(page) = weak.upgrade() {
                page.apply_permissions(*actions);
            }
        });
    }

    fn apply_permissions(&self, permitted: service::power::PowerActions) {
        for (action, tile) in &self.tiles {
            // Offering an action logind will refuse is worse than disabling it.
            let allowed = match action {
                SessionAction::Reboot => permitted.can_reboot,
                SessionAction::PowerOff => permitted.can_power_off,
                SessionAction::Suspend => permitted.can_suspend,
                _ => true,
            };
            tile.set_sensitive(allowed);
        }
    }

    fn on_clicked(self: &Rc<Self>, action: SessionAction) {
        let Some((question, detail)) = action.confirmation() else {
            perform(action);
            return;
        };

        clear_box(&self.confirm);

        let text = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
        text.set_hexpand(true);
        let title = gtk4::Label::new(Some(question));
        title.set_xalign(0.0);
        title.add_css_class("ra-tile-title");
        text.append(&title);
        if let Some(detail) = detail {
            let sub = gtk4::Label::new(Some(detail));
            sub.set_xalign(0.0);
            sub.add_css_class("ra-tile-subtitle");
            text.append(&sub);
        }
        self.confirm.append(&text);

        let yes = ui::button(&["ra-round-button"]);
        crate::icons::set_on_button(&yes, "emblem-ok-symbolic", 16);
        ui::tooltip(&yes, "Confirm");
        let no = ui::button(&["ra-round-button"]);
        crate::icons::set_on_button(&no, "window-close-symbolic", 16);
        ui::tooltip(&no, "Cancel");
        self.confirm.append(&no);
        self.confirm.append(&yes);

        let for_yes = self.clone();
        yes.connect_clicked(move |_| {
            for_yes.cancel();
            perform(action);
        });
        let for_no = self.clone();
        no.connect_clicked(move |_| for_no.cancel());

        self.grid.set_visible(false);
        self.confirm.set_visible(true);
    }

    fn cancel(&self) {
        clear_box(&self.confirm);
        self.confirm.set_visible(false);
        self.grid.set_visible(true);
    }
}

/// Remove every child of a box.
fn clear_box(box_: &gtk4::Box) {
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
}

#[derive(Clone, Copy)]
enum SessionAction {
    Lock,
    Logout,
    Suspend,
    Reboot,
    PowerOff,
    Screensaver,
}

impl SessionAction {
    /// The question to ask, or `None` for an action that needs no confirmation.
    fn confirmation(self) -> Option<(&'static str, Option<&'static str>)> {
        match self {
            Self::Reboot => Some(("Restart the system?", Some("Running applications will be closed."))),
            Self::PowerOff => Some(("Shut the system down?", Some("Running applications will be closed."))),
            Self::Logout => Some(("Log out?", Some("Unsaved work may be lost."))),
            Self::Lock => Some(("Lock the screen?", None)),
            Self::Suspend | Self::Screensaver => None,
        }
    }
}

fn perform(action: SessionAction) {
    let power = service::power::get();
    match action {
        SessionAction::Lock => spawn_detached("xfce4-session-lock"),
        SessionAction::Screensaver => spawn_detached("xfce4-screensaver-preferences"),
        SessionAction::Suspend => power.suspend(),
        SessionAction::Reboot => power.reboot(),
        SessionAction::PowerOff => power.power_off(),
        SessionAction::Logout => power.logout(),
    }
}

/// Launch an XFCE helper without a shell and without waiting for it.
fn spawn_detached(program: &str) {
    match gtk4::gio::Subprocess::newv(&[std::ffi::OsStr::new(program)], gtk4::gio::SubprocessFlags::NONE) {
        Ok(_) => info!("launched {program}"),
        Err(e) => warn!("cannot launch {program}: {e}"),
    }
}

fn about_page() -> gtk4::Box {
    let root = ui::vbox(8);
    root.set_size_request(-1, 200);

    let header = PageHeader::new();
    header.set_title("ra");
    root.append(&header.root);

    let config = crate::config::user_config_path();
    let rows: [(&str, String); 5] = [
        ("Version", env!("CARGO_PKG_VERSION").to_string()),
        ("Config", config.display().to_string()),
        ("GTK", gtk4::major_version().to_string()),
        (
            "Layer shell",
            if layer::supported() { "available" } else { "unavailable (floating windows)" }.to_string(),
        ),
        ("Session", std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown".into())),
    ];
    for (key, value) in rows {
        let line = ui::hbox(8);
        let k = gtk4::Label::new(Some(key));
        k.set_xalign(0.0);
        k.set_hexpand(true);
        k.add_css_class("ra-tile-subtitle");
        let v = gtk4::Label::new(Some(&value));
        v.set_xalign(1.0);
        v.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        v.add_css_class("ra-tile-subtitle");
        line.append(&k);
        line.append(&v);
        root.append(&line);
    }
    root
}

// ---------------------------------------------------------------------------
// List helpers
// ---------------------------------------------------------------------------

fn clear_list(list: &gtk4::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn message_row(text: &str) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    row.set_selectable(false);
    let label = gtk4::Label::new(Some(text));
    label.add_css_class("ra-empty");
    row.set_child(Some(&label));
    row
}

// ---------------------------------------------------------------------------
// Singleton
// ---------------------------------------------------------------------------

thread_local! {
    static SERVICE: RefCell<Option<Rc<QuickSettings>>> = const { RefCell::new(None) };
}

/// Create the process-wide popup. Called once, by the application.
pub fn install(app: &gtk4::Application, cfg: &QsConfig, close_on_blur: bool) -> Rc<QuickSettings> {
    let qs = QuickSettings::new(app, cfg, close_on_blur);
    info!("quick settings ready");
    SERVICE.with(|slot| *slot.borrow_mut() = Some(qs.clone()));
    qs
}

/// Toggle the popup. Called by every indicator module.
/// Tiles the quick settings knows how to draw.
///
/// See [`QuickSettings::known_tiles`].
pub fn known_tiles() -> &'static [&'static str] {
    QuickSettings::known_tiles()
}

pub fn toggle() {
    let qs = SERVICE.with(|slot| slot.borrow().clone());
    if let Some(qs) = qs {
        qs.toggle();
    }
}

/// A handle for modules that want to open the popup directly.
pub fn handle() -> Option<Rc<QuickSettings>> {
    SERVICE.with(|slot| slot.borrow().clone())
}