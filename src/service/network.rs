//! NetworkManager over D-Bus.
//!
//! Driven entirely by `PropertiesChanged` and NetworkManager's own signals;
//! there is no polling anywhere in this file.
//!
//! Bus map (verified against NetworkManager 1.56):
//!
//! | Destination | Path | Interface |
//! |---|---|---|
//! | `org.freedesktop.NetworkManager` | `/org/freedesktop/NetworkManager` | `.NetworkManager` |
//! | " | `<device>` | `.Device`, `.Device.Wireless` |
//! | " | `<ap>` | `.AccessPoint` |
//! | " | `/org/freedesktop/NetworkManager/Settings` | `.Settings` |
//! | " | `<connection>` | `.Settings.Connection` |

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::ToVariant;

use crate::dbus::{self, Bus, Emitter, Subscription};
use crate::{debug, info, warn};

const BUS: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const IFACE: &str = "org.freedesktop.NetworkManager";
const IFACE_DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const IFACE_WIFI: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const IFACE_AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
const IFACE_SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";

/// `DeviceType` values that matter to a panel.
const DEVICE_TYPE_WIFI: u32 = 2;
const DEVICE_TYPE_ETHERNET: u32 = 1;

/// NetworkManager device states that matter.
const DEVICE_STATE_ACTIVATED: u32 = 100;
/// Global state: connected.
const STATE_CONNECTED_GLOBAL: u32 = 70;

/// One visible network.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessPoint {
    pub path: String,
    pub ssid: String,
    /// 0-100.
    pub strength: u8,
    pub secure: bool,
    pub connected: bool,
    /// NetworkManager has a saved profile, so joining needs no passphrase.
    pub saved: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetworkState {
    /// NetworkManager is on the bus and answering.
    pub available: bool,
    pub wifi_present: bool,
    pub wifi_enabled: bool,
    pub ssid: Option<String>,
    /// 0-100.
    pub strength: u8,
    pub ethernet: bool,
    pub state: u32,
}

impl NetworkState {
    pub fn is_connected(&self) -> bool {
        self.state == STATE_CONNECTED_GLOBAL
    }

    /// Text for the bar button and the quick-settings tile.
    pub fn label(&self) -> String {
        if !self.available {
            return "Network".into();
        }
        if self.is_connected() {
            match &self.ssid {
                Some(s) => s.clone(),
                None if self.ethernet => "Ethernet".into(),
                None => "Connected".into(),
            }
        } else if !self.wifi_enabled && self.wifi_present {
            "Wi-Fi off".into()
        } else {
            "No network".into()
        }
    }

    /// Adwaita icon name for the current state.
    pub fn icon(&self) -> &'static str {
        if !self.available {
            return "network-offline-symbolic";
        }
        if self.is_connected() {
            if self.ssid.is_none() && self.ethernet {
                return "network-wired-symbolic";
            }
            return match self.strength {
                80..=100 => "network-wireless-signal-excellent-symbolic",
                55..=79 => "network-wireless-signal-good-symbolic",
                30..=54 => "network-wireless-signal-ok-symbolic",
                _ => "network-wireless-signal-weak-symbolic",
            };
        }
        if self.wifi_present && !self.wifi_enabled {
            "network-wireless-off-symbolic"
        } else {
            "network-offline-symbolic"
        }
    }
}

pub struct Network {
    state: RefCell<NetworkState>,
    emitter: Emitter<NetworkState>,
    bus: Option<Bus>,
    /// Object path of the Wi-Fi device, once discovered.
    wifi_device: RefCell<Option<String>>,
    /// Saved profiles: SSID -> connection object path.
    saved: RefCell<HashMap<String, String>>,
    subs: RefCell<Vec<gio::SignalSubscription>>,
}

impl Network {
    fn new(bus: Option<Bus>) -> Rc<Self> {
        let me = Rc::new(Self {
            state: RefCell::new(NetworkState::default()),
            emitter: Emitter::new(),
            bus,
            wifi_device: RefCell::new(None),
            saved: RefCell::new(HashMap::new()),
            subs: RefCell::new(Vec::new()),
        });
        me.connect_signals();
        if let Some(bus) = &me.bus {
            let bus = bus.clone();
            crate::dbus::spawn(load_saved(bus.clone(), Rc::downgrade(&me)));
            crate::dbus::spawn(refresh(bus, Rc::downgrade(&me)));
        }
        me
    }

    fn connect_signals(self: &Rc<Self>) {
        let Some(bus) = self.bus.clone() else { return };
        let mut subs = Vec::new();

        // Root object: device hotplug, radio state, global state.
        subs.push(bus.subscribe(
            Some(BUS),
            Some(dbus::PROPERTIES),
            Some("PropertiesChanged"),
            Some(ROOT),
            {
                let weak = Rc::downgrade(self);
                let bus = bus.clone();
                move |sig| {
                    let (iface, changed) = changed_properties(&sig);
                    if iface == IFACE
                        && changed.iter().any(|k| k == "Devices" || k == "WirelessEnabled" || k == "State")
                    {
                        spawn_refresh(&bus, &weak);
                    }
                }
            },
        ));
        for member in ["DeviceAdded", "DeviceRemoved", "StateChanged"] {
            subs.push(bus.subscribe(
                Some(BUS),
                Some(IFACE),
                Some(member),
                Some(ROOT),
                {
                    let weak = Rc::downgrade(self);
                    let bus = bus.clone();
                    move |_| spawn_refresh(&bus, &weak)
                },
            ));
        }

        // Per-device changes. Filtering on the Wi-Fi device path keeps a
        // dual-radio or dock-and-UNDOCK from triggering needless refreshes.
        subs.push(bus.subscribe(
            Some(BUS),
            Some(dbus::PROPERTIES),
            Some("PropertiesChanged"),
            None,
            {
                let weak = Rc::downgrade(self);
                let bus = bus.clone();
                move |sig| {
                    let iface = sig.parameters.child_value(0).get::<String>().unwrap_or_default();
                    if iface != IFACE_DEVICE && iface != IFACE_WIFI {
                        return;
                    }
                    if let Some(me) = weak.upgrade() {
                        if let Some(dev) = me.wifi_device.borrow().clone() {
                            if sig.object_path != dev {
                                return;
                            }
                        }
                    }
                    spawn_refresh(&bus, &weak);
                }
            },
        ));

        // Active access point properties: strength and SSID.
        subs.push(bus.subscribe(
            Some(BUS),
            Some(dbus::PROPERTIES),
            Some("PropertiesChanged"),
            None,
            {
                let weak = Rc::downgrade(self);
                let bus = bus.clone();
                move |sig| {
                    let iface = sig.parameters.child_value(0).get::<String>().unwrap_or_default();
                    if iface != IFACE_AP {
                        return;
                    }
                    spawn_refresh(&bus, &weak);
                }
            },
        ));

        // Saved-profile bookkeeping: both the signals and the `Connections`
        // property, because NM emits one or the other depending on how a
        // profile was edited.
        for member in ["NewConnection", "ConnectionRemoved"] {
            subs.push(bus.subscribe(
                Some(BUS),
                Some(IFACE_SETTINGS),
                Some(member),
                Some(SETTINGS_PATH),
                {
                    let weak = Rc::downgrade(self);
                    let bus = bus.clone();
                    move |_| crate::dbus::spawn(load_saved(bus.clone(), weak.clone()))
                },
            ));
        }
        subs.push(bus.subscribe(
            Some(BUS),
            Some(IFACE_SETTINGS),
            None,
            Some(SETTINGS_PATH),
            {
                let weak = Rc::downgrade(self);
                let bus = bus.clone();
                move |_| crate::dbus::spawn(load_saved(bus.clone(), weak.clone()))
            },
        ));

        // Survive `systemctl restart NetworkManager`.
        {
            let watch_bus = bus.clone();
            subs.push(bus.watch_name(BUS, {
                let weak = Rc::downgrade(self);
                move |owned| {
                    let Some(me) = weak.upgrade() else { return };
                    if owned {
                        info!("NetworkManager is available");
                        let weak = Rc::downgrade(&me);
                        let bus = watch_bus.clone();
                        crate::dbus::spawn(async {
                            load_saved(bus.clone(), weak.clone()).await;
                            refresh(bus, weak).await;
                        });
                    } else {
                        warn!("NetworkManager went away; showing no network");
                        me.saved.borrow_mut().clear();
                        me.replace_state(NetworkState::default());
                    }
                }
            }));
        }

        *self.subs.borrow_mut() = subs;
    }

    fn replace_state(&self, next: NetworkState) {
        let mut current = self.state.borrow_mut();
        if *current == next {
            return;
        }
        *current = next.clone();
        drop(current);
        debug!("network: {} ({})", next.label(), next.icon());
        self.emitter.emit(&next);
    }

    pub fn state(&self) -> std::cell::Ref<'_, NetworkState> {
        self.state.borrow()
    }

    pub fn on_change(&self, f: impl Fn(&NetworkState) + 'static) -> Subscription {
        self.emitter.subscribe(f)
    }

    pub fn is_available(&self) -> bool {
        self.bus.is_some()
    }

    // -- actions ------------------------------------------------------------

    /// Turn the Wi-Fi radio on or off.
    pub fn set_wifi_enabled(&self, enabled: bool) {
        let Some(bus) = &self.bus else { return };
        let body = glib::Variant::tuple_from_iter([&IFACE.to_variant(), &"WirelessEnabled".to_variant(), &enabled.to_variant()]);
        bus.call_no_reply(BUS, ROOT, dbus::PROPERTIES, "Set", Some(&body));
    }

    /// Ask NetworkManager to rescan. NM rate-limits this and may refuse, which
    /// is expected; the resulting `PropertiesChanged` updates the list anyway.
    pub fn request_scan(&self) {
        let (Some(bus), Some(dev)) = (&self.bus, self.wifi_device.borrow().clone()) else { return };
        if !self.state.borrow().wifi_enabled {
            return;
        }
        let body = glib::Variant::tuple_from_iter([&dbus::vardict(Vec::new())]);
        bus.call_no_reply(BUS, &dev, IFACE_WIFI, "RequestScan", Some(&body));
    }

    /// Disconnect the active Wi-Fi connection.
    pub fn disconnect(&self) {
        let (Some(bus), Some(dev)) = (&self.bus, self.wifi_device.borrow().clone()) else { return };
        bus.call_no_reply(BUS, &dev, IFACE_DEVICE, "Disconnect", None);
    }

    /// Every visible network, strongest first.
    ///
    /// This costs one D-Bus round trip per access point, which is why it is a
    /// pull rather than part of the pushed state: it only runs when the Wi-Fi
    /// page is actually opened.
    pub async fn list_access_points(&self) -> Vec<AccessPoint> {
        let (Some(bus), Some(dev)) = (&self.bus, self.wifi_device.borrow().clone()) else { return Vec::new() };
        if !self.state.borrow().wifi_enabled {
            return Vec::new();
        }
        let paths = match bus.get(BUS, &dev, IFACE_WIFI, "AccessPoints").await {
            Ok(v) => dbus::as_strv(&v),
            Err(e) => {
                debug!("AccessPoints: {e}");
                return Vec::new();
            }
        };

        let mut raw = Vec::with_capacity(paths.len());
        for path in &paths {
            let Ok(props) = bus.get_all(BUS, path, IFACE_AP).await else { continue };
            let Some(ssid) = props.get("Ssid").and_then(dbus::as_bytes_str) else { continue };
            if ssid.trim().is_empty() {
                continue;
            }
            let flags = props.get("Flags").and_then(dbus::as_u32).unwrap_or(0);
            let wpa = props.get("WpaFlags").and_then(dbus::as_u32).unwrap_or(0);
            let rsn = props.get("RsnFlags").and_then(dbus::as_u32).unwrap_or(0);
            raw.push(AccessPoint {
                path: path.clone(),
                saved: self.saved.borrow().contains_key(&ssid),
                ssid,
                strength: props.get("Strength").and_then(dbus::as_u8).unwrap_or(0),
                secure: (flags & 0x1) != 0 || wpa != 0 || rsn != 0,
                connected: false,
            });
        }

        // 2.4 and 5 GHz radios both see the same SSID. Show one entry, keeping
        // the stronger. Path is the tie-break so ordering never jitters.
        raw.sort_by(|a, b| b.strength.cmp(&a.strength).then_with(|| a.path.cmp(&b.path)));
        let mut merged: Vec<AccessPoint> = Vec::with_capacity(raw.len());
        for ap in raw {
            match merged.iter_mut().find(|m| m.ssid == ap.ssid) {
                Some(existing) => {
                    existing.saved |= ap.saved;
                    existing.secure |= ap.secure;
                }
                None => merged.push(ap),
            }
        }

        // Mark the active network.
        let active = self.state.borrow().ssid.clone();
        if let Some(ssid) = active {
            for ap in merged.iter_mut().filter(|ap| ap.ssid == ssid) {
                ap.connected = true;
            }
        }
        merged
    }

    /// Join a network.
    ///
    /// * no passphrase - activates a saved profile, or joins an open network;
    /// * `Some(psk)` - creates a profile with that passphrase.
    pub fn connect(self: &Rc<Self>, ap: &AccessPoint, passphrase: Option<&str>) {
        let Some(bus) = self.bus.clone() else { return };
        let Some(dev) = self.wifi_device.borrow().clone() else {
            warn!("no Wi-Fi device to connect with");
            return;
        };
        let ap = ap.clone();
        let saved = self.saved.borrow().get(&ap.ssid).cloned();
        // Own the passphrase so the future does not borrow the caller's stack.
        let passphrase = passphrase.map(str::to_owned);

        crate::dbus::spawn(async move {
            if passphrase.is_none() {
                if let Some(conn) = saved {
                    let body = glib::Variant::tuple_from_iter([
                        &conn.to_variant(),
                        &dev.to_variant(),
                        &ap.path.to_variant(),
                    ]);
                    if let Err(e) = bus.call(BUS, ROOT, IFACE, "ActivateConnection", Some(&body)).await {
                        warn!("ActivateConnection({}): {e}", ap.ssid);
                    } else {
                        info!("activating saved profile for `{}`", ap.ssid);
                    }
                    return;
                }
            }

            if ap.secure && passphrase.is_none() {
                warn!("`{}` is secured and has no saved profile", ap.ssid);
                return;
            }

            let body = glib::Variant::tuple_from_iter([
                &build_profile(&ap.ssid, passphrase.as_deref()),
                &dev.to_variant(),
                &ap.path.to_variant(),
                &"".to_variant(), // specific_object: ignored when adding
                &false.to_variant(),
            ]);
            match bus.call(BUS, ROOT, IFACE, "AddAndActivateConnection2", Some(&body)).await {
                Ok(_) => info!("joining `{}`", ap.ssid),
                Err(e) => warn!("AddAndActivateConnection2({}): {e}", ap.ssid),
            }
        });
    }
}

/// Build a connection profile for a network with no saved configuration.
///
/// This is the same shape `nmcli connection add` produces. A supplied
/// passphrase crosses the system bus to NetworkManager and is written to the
/// system connection store *by NetworkManager*, behind polkit, exactly as for
/// any other network client. `ra` never stores or caches it.
fn build_profile(ssid: &str, passphrase: Option<&str>) -> glib::Variant {
    let connection = dbus::vardict(vec![
        ("id".into(), glib::Variant::from(ssid)),
        ("type".into(), glib::Variant::from("802-11-wireless")),
        // Let a pre-existing user profile for the same network win on reconnect.
        ("autoconnect-retries".into(), glib::Variant::from(3i32)),
    ]);

    let mut wireless = vec![
        ("ssid".to_string(), glib::Variant::from(ssid.as_bytes().to_vec())),
        ("mode".to_string(), glib::Variant::from("infrastructure")),
    ];

    match passphrase.filter(|p| !p.is_empty()) {
        Some(psk) => {
            let security = dbus::vardict(vec![
                ("key-mgmt".into(), glib::Variant::from("wpa-psk")),
                ("psk".into(), glib::Variant::from(psk)),
            ]);
            wireless.push(("security".to_string(), security));
        }
        None => wireless.push(("key-mgmt".to_string(), glib::Variant::from("none"))),
    }

    dbus::vardict(vec![
        ("connection".to_string(), connection),
        ("802-11-wireless".to_string(), dbus::vardict(wireless)),
        // A profile created from the panel is transient: the user did not ask
        // for it to join their system connections, and leaving it behind would
        // make the saved list grow every time they try a network.
        ("add-unsaved".to_string(), glib::Variant::from(true)),
    ])
}

/// Re-read state. Five round trips in the common case; triggered only by a
/// real change event, never on a timer.
async fn refresh(bus: Bus, weak: Weak<Network>) {
    let Some(me) = weak.upgrade() else { return };
    let mut next = NetworkState { available: true, ..Default::default() };

    let Ok(root) = bus.get_all(BUS, ROOT, IFACE).await else {
        me.replace_state(NetworkState::default());
        return;
    };
    next.wifi_enabled = root.get("WirelessEnabled").and_then(dbus::as_bool).unwrap_or(false);
    next.state = root.get("State").and_then(dbus::as_u32).unwrap_or(0);

    let devices = root.get("Devices").map(|v| dbus::as_strv(&v)).unwrap_or_default();
    let mut wifi = None;
    for dev in devices {
        let Ok(props) = bus.get_all(BUS, &dev, IFACE_DEVICE).await else { continue };
        match props.get("DeviceType").and_then(dbus::as_u32).unwrap_or(0) {
            DEVICE_TYPE_WIFI => {
                next.wifi_present = true;
                wifi = Some(dev);
            }
            DEVICE_TYPE_ETHERNET => {
                let device_state = props.get("State").and_then(dbus::as_u32).unwrap_or(0);
                next.ethernet = device_state == DEVICE_STATE_ACTIVATED;
            }
            _ => {}
        }
    }
    *me.wifi_device.borrow_mut() = wifi.clone();

    if next.wifi_present && next.wifi_enabled {
        if let Some(dev) = wifi {
            if let Ok(props) = bus.get_all(BUS, &dev, IFACE_WIFI).await {
                if let Some(ap) = props.get("ActiveAccessPoint").and_then(dbus::as_path) {
                    if let Ok(ap_props) = bus.get_all(BUS, &ap, IFACE_AP).await {
                        next.ssid = ap_props.get("Ssid").and_then(dbus::as_bytes_str);
                        next.strength = ap_props.get("Strength").and_then(dbus::as_u8).unwrap_or(0);
                    }
                }
            }
        }
    }

    me.replace_state(next);
}

/// Map SSID -> saved connection object path.
async fn load_saved(bus: Bus, weak: Weak<Network>) {
    let Some(me) = weak.upgrade() else { return };
    let Ok(conns) = bus.call(BUS, SETTINGS_PATH, IFACE_SETTINGS, "ListConnections", None).await else {
        me.saved.borrow_mut().clear();
        return;
    };
    let conns = conns.map(|v| dbus::as_strv(&v)).unwrap_or_default();

    let mut map = HashMap::with_capacity(conns.len());
    for conn in conns {
        let Ok(settings) = bus.call(BUS, &conn, IFACE_SETTINGS, "GetSettings", None).await else { continue };
        let Some(settings) = settings else { continue };
        let settings = dbus::variant_to_dict(settings);
        let Some(wireless) = settings.get("802-11-wireless") else { continue };
        let wireless = dbus::variant_to_dict(wireless.clone());
        let Some(ssid) = wireless.get("ssid").and_then(dbus::as_bytes_str) else { continue };
        map.insert(ssid, conn);
    }
    debug!("{} saved Wi-Fi profile(s)", map.len());
    *me.saved.borrow_mut() = map;
}

fn spawn_refresh(bus: &Bus, weak: &Weak<Network>) {
    let bus = bus.clone();
    let weak = weak.clone();
    crate::dbus::spawn(refresh(bus, weak));
}

/// Extract `(interface, changed-keys)` from a `PropertiesChanged` payload.
fn changed_properties(sig: &gio::DBusSignalRef<'_>) -> (String, Vec<String>) {
    let iface = sig.parameters.child_value(0).get::<String>().unwrap_or_default();
    let changed = sig.parameters.child_value(1);
    let keys = changed.iter().filter_map(|c| c.child_value(0).get::<String>()).collect();
    (iface, keys)
}

// ---------------------------------------------------------------------------
// Singleton
// ---------------------------------------------------------------------------

thread_local! {
    static SERVICE: RefCell<Option<Rc<Network>>> = const { RefCell::new(None) };
}

/// The process-wide NetworkManager service, started on first use so a config
/// that does not mention the network module never pays for it.
pub fn get() -> Rc<Network> {
    SERVICE.with(|slot| {
        if let Some(svc) = slot.borrow().as_ref() {
            return svc.clone();
        }
        let svc = match Bus::system() {
            Ok(bus) => Network::new(Some(bus)),
            Err(e) => {
                warn!("system bus unreachable, network disabled: {e}");
                Network::new(None)
            }
        };
        *slot.borrow_mut() = Some(svc.clone());
        svc
    })
}