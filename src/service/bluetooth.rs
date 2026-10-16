//! BlueZ over D-Bus, using the standard `ObjectManager` at `/`.
//!
//! A single `GetManagedObjects` call yields every adapter and paired device
//! with all of their properties, so initial state and every later update
//! (`InterfacesAdded`, `InterfacesRemoved`, `PropertiesChanged`) arrives without
//! any per-device round trips. There is no polling.
//!
//! Bus map (verified against BlueZ 5.x):
//!
//! | Destination | Path | Interface |
//! |---|---|---|
//! | `org.bluez` | `/` | `org.freedesktop.DBus.ObjectManager` |
//! | " | `/org/bluez/hciN` | `org.bluez.Adapter1` |
//! | " | `/org/bluez/hciN/dev_XX_...` | `org.bluez.Device1` |

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use gtk4::gio;
use gtk4::prelude::ToVariant;

use crate::dbus::{self, Bus, Emitter, Subscription};
use crate::{debug, info, warn};

const BUS: &str = "org.bluez";
const ROOT_PATH: &str = "/";
const IFACE_ADAPTER: &str = "org.bluez.Adapter1";
const IFACE_DEVICE: &str = "org.bluez.Device1";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BluetoothState {
    pub available: bool,
    /// An adapter exists and reports itself powered.
    pub present: bool,
    pub powered: bool,
    /// True while a discovery scan is running.
    pub discovering: bool,
    /// At least one device is connected.
    pub any_connected: bool,
    /// Number of paired devices.
    pub paired: usize,
}

impl BluetoothState {
    pub fn label(&self) -> String {
        if !self.available {
            return "Bluetooth".into();
        }
        if !self.present {
            return "No adapter".into();
        }
        if !self.powered {
            return "Bluetooth off".into();
        }
        if self.discovering {
            return "Scanning...".into();
        }
        if self.any_connected {
            return "Connected".into();
        }
        "Bluetooth".into()
    }

    pub fn icon(&self) -> &'static str {
        if !self.available || !self.present || !self.powered {
            return "bluetooth-disabled-symbolic";
        }
        if self.any_connected {
            "bluetooth-active-symbolic"
        } else {
            "bluetooth-symbolic"
        }
    }
}

/// One device in the Bluetooth page.
#[derive(Debug, Clone, PartialEq)]
pub struct BluetoothDevice {
    pub path: String,
    pub address: String,
    pub name: String,
    pub connected: bool,
    pub paired: bool,
    pub battery: Option<u8>,
    /// Icon hint BlueZ derived from the device class, e.g. `audio-headset`.
    pub icon: String,
}

pub struct Bluetooth {
    state: RefCell<BluetoothState>,
    emitter: Emitter<BluetoothState>,
    /// Object paths of every adapter.
    adapters: RefCell<Vec<String>>,
    bus: Option<Bus>,
    subs: RefCell<Vec<gio::SignalSubscription>>,
}

impl Bluetooth {
    fn new(bus: Option<Bus>) -> Rc<Self> {
        let me = Rc::new(Self {
            state: RefCell::new(BluetoothState::default()),
            emitter: Emitter::new(),
            adapters: RefCell::new(Vec::new()),
            bus,
            subs: RefCell::new(Vec::new()),
        });
        me.connect_signals();
        if me.bus.is_some() {
            let weak = Rc::downgrade(&me);
            crate::dbus::spawn(reload(weak));
        }
        me
    }

    fn connect_signals(self: &Rc<Self>) {
        let Some(bus) = self.bus.clone() else { return };
        let mut subs = Vec::new();

        // One subscription covers the whole tree: BlueZ emits these from `/`.
        for member in ["InterfacesAdded", "InterfacesRemoved"] {
            subs.push(bus.subscribe(Some(BUS), Some(dbus::OBJECT_MANAGER), Some(member), Some(ROOT_PATH), {
                let weak = Rc::downgrade(self);
                move |_| crate::dbus::spawn(reload(weak.clone()))
            }));
        }
        subs.push(bus.subscribe(Some(BUS), Some(dbus::PROPERTIES), Some("PropertiesChanged"), None, {
            let weak = Rc::downgrade(self);
            move |_| crate::dbus::spawn(reload(weak.clone()))
        }));

        subs.push(bus.watch_name(BUS, {
            let weak = Rc::downgrade(self);
            move |owned| {
                let Some(me) = weak.upgrade() else { return };
                if owned {
                    info!("bluetoothd is available");
                    crate::dbus::spawn(reload(weak.clone()));
                } else {
                    warn!("bluetoothd went away");
                    me.replace_state(BluetoothState::default());
                }
            }
        }));

        *self.subs.borrow_mut() = subs;
    }

    fn replace_state(&self, next: BluetoothState) {
        let mut current = self.state.borrow_mut();
        if *current == next {
            return;
        }
        *current = next.clone();
        drop(current);
        debug!("bluetooth: {}", next.label());
        self.emitter.emit(&next);
    }

    pub fn state(&self) -> std::cell::Ref<'_, BluetoothState> {
        self.state.borrow()
    }

    pub fn on_change(&self, f: impl Fn(&BluetoothState) + 'static) -> Subscription {
        self.emitter.subscribe(f)
    }

    /// Power every adapter on or off.
    ///
    /// All adapters are driven together: BlueZ has no notion of a "primary"
    /// adapter and a panel that only toggled one would be actively misleading
    /// on a machine with a built-in radio plus a USB dongle.
    pub fn set_powered(&self, on: bool) {
        let Some(bus) = &self.bus else { return };
        for path in self.adapters.borrow().iter() {
            bus.set_no_reply(BUS, path, IFACE_ADAPTER, "Powered", &on.to_variant());
        }
    }

    pub fn set_discovering(&self, on: bool) {
        let Some(bus) = &self.bus else { return };
        for path in self.adapters.borrow().iter() {
            let method = if on { "StartDiscovery" } else { "StopDiscovery" };
            bus.call_no_reply(BUS, path, IFACE_ADAPTER, method, None);
        }
    }

    pub fn connect_device(&self, path: &str) {
        let Some(bus) = &self.bus else { return };
        bus.call_no_reply(BUS, path, IFACE_DEVICE, "Connect", None);
    }

    pub fn disconnect_device(&self, path: &str) {
        let Some(bus) = &self.bus else { return };
        bus.call_no_reply(BUS, path, IFACE_DEVICE, "Disconnect", None);
    }

    /// All known devices, connected first then by name.
    pub async fn list_devices(&self) -> Vec<BluetoothDevice> {
        let Some(bus) = &self.bus else { return Vec::new() };
        let Ok(objects) = bus.call(BUS, ROOT_PATH, dbus::OBJECT_MANAGER, "GetManagedObjects", None).await else {
            return Vec::new();
        };
        // The reply is a one-element tuple around the object map.
        let Some(objects) = objects.map(dbus::reply_value) else { return Vec::new() };

        let mut out = Vec::new();
        for entry in objects.iter() {
            let path = entry.child_value(0).get::<String>().unwrap_or_default();
            let device_props = dbus::variant_to_dict(entry.child_value(1)).get(IFACE_DEVICE).cloned();
            let Some(props) = device_props else { continue };
            let props = dbus::variant_to_dict(props);
            let address = props.get("Address").and_then(dbus::as_str).unwrap_or_default();
            // `Alias` is user-settable and falls back to `Name` in BlueZ.
            let name = props
                .get("Alias")
                .and_then(dbus::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| props.get("Name").and_then(dbus::as_str))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| address.clone());
            out.push(BluetoothDevice {
                path,
                address,
                name,
                connected: props.get("Connected").and_then(dbus::as_bool).unwrap_or(false),
                paired: props.get("Paired").and_then(dbus::as_bool).unwrap_or(false),
                battery: props.get("BatteryPercentage").and_then(dbus::as_u8),
                icon: props.get("Icon").and_then(dbus::as_str).unwrap_or_else(|| "bluetooth-symbolic".into()),
            });
        }
        out.sort_by(|a, b| b.connected.cmp(&a.connected).then_with(|| a.name.cmp(&b.name)));
        out
    }
}

/// Rebuild the pushed state from a single `GetManagedObjects` call.
async fn reload(weak: Weak<Bluetooth>) {
    let Some(me) = weak.upgrade() else { return };
    let Some(bus) = me.bus.clone() else { return };

    let Ok(objects) = bus.call(BUS, ROOT_PATH, dbus::OBJECT_MANAGER, "GetManagedObjects", None).await else {
        me.replace_state(BluetoothState::default());
        return;
    };
    let Some(objects) = objects.map(dbus::reply_value) else {
        me.replace_state(BluetoothState::default());
        return;
    };

    let mut adapters = Vec::new();
    let mut powered = false;
    let mut discovering = false;
    let mut connected = false;
    let mut paired = 0usize;

    for entry in objects.iter() {
        let path = entry.child_value(0).get::<String>().unwrap_or_default();
        // `entry` is `{oa{sa{sv}}}`: the path, then the interface map.
        let ifaces = entry.child_value(1);

        if let Some(adapter) = dbus::variant_to_dict(ifaces.clone()).get(IFACE_ADAPTER).cloned() {
            adapters.push(path);
            let adapter = dbus::variant_to_dict(adapter);
            powered |= adapter.get("Powered").and_then(dbus::as_bool).unwrap_or(false);
            discovering |= adapter.get("Discovering").and_then(dbus::as_bool).unwrap_or(false);
            continue;
        }

        if let Some(device) = dbus::variant_to_dict(ifaces.clone()).get(IFACE_DEVICE).cloned() {
            let device = dbus::variant_to_dict(device);
            connected |= device.get("Connected").and_then(dbus::as_bool).unwrap_or(false);
            paired += usize::from(device.get("Paired").and_then(dbus::as_bool).unwrap_or(false));
        }
    }

    *me.adapters.borrow_mut() = adapters.clone();
    me.replace_state(BluetoothState {
        available: true,
        present: !adapters.is_empty(),
        powered,
        discovering,
        any_connected: connected,
        paired,
    });
}

thread_local! {
    static SERVICE: RefCell<Option<Rc<Bluetooth>>> = const { RefCell::new(None) };
}

/// The process-wide BlueZ service, started on first use.
pub fn get() -> Rc<Bluetooth> {
    SERVICE.with(|slot| {
        if let Some(svc) = slot.borrow().as_ref() {
            return svc.clone();
        }
        let svc = match Bus::system() {
            Ok(bus) => Bluetooth::new(Some(bus)),
            Err(e) => {
                warn!("system bus unreachable, bluetooth disabled: {e}");
                Bluetooth::new(None)
            }
        };
        *slot.borrow_mut() = Some(svc.clone());
        svc
    })
}