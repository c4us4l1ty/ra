//! Battery, power profiles and power actions.
//!
//! Three D-Bus destinations, each chosen at runtime so the panel works on
//! distributions that ship either UPower or `power-profiles-daemon`:
//!
//! * battery - `org.freedesktop.UPower`, discovered through `InterfacesAdded`
//!   on `/org/freedesktop/UPower` so hot-plugged batteries are picked up;
//! * profiles - `net.hadess.PowerProfiles`, falling back to
//!   `org.freedesktop.UPower.PowerProfiles`, falling back to nothing;
//! * actions - `org.freedesktop.login1`.
//!
//! Every one of these pushes state changes, so there is no polling.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::ToVariant;

use crate::dbus::{self, Bus, Emitter, Subscription};
use crate::{debug, warn};

const UPOWER: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const UPOWER_DEVICE: &str = "org.freedesktop.UPower.Device";

const HADESS: &str = "net.hadess.PowerProfiles";
const HADESS_PATH: &str = "/net/hadess/PowerProfiles";
const UPOWER_PROFILES: &str = "org.freedesktop.UPower.PowerProfiles";
const UPOWER_PROFILES_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";

const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const LOGIND_MANAGER: &str = "org.freedesktop.login1.Manager";

// -- battery -----------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatteryState {
    /// A battery was found and reports itself present.
    pub present: bool,
    /// 0-100.
    pub percentage: u8,
    pub charging: bool,
    /// Minutes remaining, when UPower can estimate it.
    pub time_to_empty: Option<i64>,
    /// UPower `WarningLevel`: 1 none, 2 discharge, 3 critical.
    pub warning_level: u32,
}

impl BatteryState {
    pub fn icon(&self) -> &'static str {
        if !self.present {
            return "battery-missing-symbolic";
        }
        let base = match self.percentage {
            95..=100 => "battery-full",
            80..=94 => "battery-good",
            55..=79 => "battery-medium",
            30..=54 => "battery-low",
            _ => "battery-empty",
        };
        if self.charging {
            return if base == "battery-full" { "battery-full-charging-symbolic" } else { "battery-not-charging-symbolic" };
        }
        // `-symbolic` names exist for the not-charging variants only; reuse the
        // plain one where there is no matching pair.
        match base {
            "battery-full" => "battery-full-symbolic",
            "battery-good" => "battery-good-symbolic",
            "battery-medium" => "battery-medium-symbolic",
            "battery-low" => "battery-low-symbolic",
            _ => "battery-empty-symbolic",
        }
    }

    pub fn label(&self) -> String {
        if !self.present {
            return "No battery".into();
        }
        if self.charging {
            return format!("{}%", self.percentage);
        }
        match self.time_to_empty {
            Some(mins) if mins > 0 => {
                let h = mins / 60;
                let m = mins % 60;
                if h > 0 {
                    format!("{}% · {}h {}m", self.percentage, h, m)
                } else {
                    format!("{}% · {}m", self.percentage, m)
                }
            }
            _ => format!("{}%", self.percentage),
        }
    }
}

// -- power profiles ----------------------------------------------------------

/// The profiles the running system actually offers. Which daemon is present is
/// discovered at runtime, so the tile shows what is real rather than a fixed
/// list of three that may not be switchable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProfileState {
    pub available: bool,
    /// Human labels in the order the daemon reports them.
    pub profiles: Vec<String>,
    pub active: Option<String>,
}

impl ProfileState {
    pub fn icon(&self) -> &'static str {
        match self.active.as_deref() {
            Some("power-saver") => "battery-saver-symbolic",
            Some("performance") => "speedometer-symbolic",
            Some(_) => "power-plug-symbolic",
            None => "power-plug-off-symbolic",
        }
    }

    pub fn label(&self) -> String {
        match &self.active {
            Some(p) if p == "power-saver" => "Power Saver".into(),
            Some(p) if p == "performance" => "Performance".into(),
            Some(p) if p == "balanced" => "Balanced".into(),
            Some(p) => p.clone(),
            None => "Power".into(),
        }
    }
}

/// What the system will let us do when the user asks.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PowerActions {
    pub can_power_off: bool,
    pub can_reboot: bool,
    pub can_suspend: bool,
}

// -- service -----------------------------------------------------------------

pub struct Power {
    battery: RefCell<BatteryState>,
    battery_emitter: Emitter<BatteryState>,
    profile: RefCell<ProfileState>,
    profile_emitter: Emitter<ProfileState>,
    actions: RefCell<PowerActions>,
    actions_emitter: Emitter<PowerActions>,
    bus: Option<Bus>,
    /// Object path of the battery we are following, if any.
    battery_path: RefCell<Option<String>>,
    /// Which profile daemon answered, if any.
    profile_daemon: RefCell<Option<(String, String)>>,
    subs: RefCell<Vec<gio::SignalSubscription>>,
}

impl Power {
    fn new(bus: Option<Bus>) -> Rc<Self> {
        let me = Rc::new(Self {
            battery: RefCell::new(BatteryState::default()),
            battery_emitter: Emitter::new(),
            profile: RefCell::new(ProfileState::default()),
            profile_emitter: Emitter::new(),
            actions: RefCell::new(PowerActions::default()),
            actions_emitter: Emitter::new(),
            bus,
            battery_path: RefCell::new(None),
            profile_daemon: RefCell::new(None),
            subs: RefCell::new(Vec::new()),
        });
        me.connect_signals();
        if let Some(bus) = me.bus.clone() {
            let weak = Rc::downgrade(&me);
            crate::dbus::spawn(async {
                find_battery(bus.clone(), weak.clone()).await;
                find_profiles(bus.clone(), weak.clone()).await;
                query_actions(bus, weak).await;
            });
        }
        me
    }

    fn connect_signals(self: &Rc<Self>) {
        let Some(bus) = self.bus.clone() else { return };
        let mut subs = Vec::new();

        // Battery hotplug: UPower lists every power device under its root.
        for member in ["InterfacesAdded", "InterfacesRemoved"] {
            subs.push(bus.subscribe(Some(UPOWER), Some(dbus::OBJECT_MANAGER), Some(member), Some(UPOWER_PATH), {
                let weak = Rc::downgrade(self);
                let bus = bus.clone();
                move |_| crate::dbus::spawn(find_battery(bus.clone(), weak.clone()))
            }));
        }

        // Battery level changes.
        subs.push(bus.subscribe(Some(UPOWER), Some(dbus::PROPERTIES), Some("PropertiesChanged"), None, {
            let weak = Rc::downgrade(self);
            let bus = bus.clone();
            move |_| crate::dbus::spawn(refresh_battery(bus.clone(), weak.clone()))
        }));
        {
            let watch_bus = bus.clone();
            subs.push(bus.watch_name(UPOWER, {
                let weak = Rc::downgrade(self);
                move |owned| {
                    let Some(me) = weak.upgrade() else { return };
                    if owned {
                        let weak = Rc::downgrade(&me);
                        let bus = watch_bus.clone();
                        crate::dbus::spawn(async {
                            find_battery(bus.clone(), weak.clone()).await;
                            refresh_battery(bus, weak).await;
                        });
                    } else {
                        me.set_battery(BatteryState::default());
                    }
                }
            }));
        }

        // Profile daemon: subscribe to whichever one turns up.
        for (name, path, iface) in [
            (HADESS, HADESS_PATH, "net.hadess.PowerProfiles"),
            (UPOWER_PROFILES, UPOWER_PROFILES_PATH, UPOWER_PROFILES),
        ] {
            subs.push(bus.subscribe(Some(name), Some(iface), Some("PropertiesChanged"), Some(path), {
                let weak = Rc::downgrade(self);
                let bus = bus.clone();
                move |_| crate::dbus::spawn(refresh_profiles(bus.clone(), weak.clone()))
            }));
            {
                let probe = bus.clone();
                subs.push(bus.watch_name(name, {
                    let weak = Rc::downgrade(self);
                    move |owned| {
                        if owned {
                            crate::dbus::spawn(find_profiles(probe.clone(), weak.clone()));
                        }
                    }
                }));
            }
        }

        // logind: session state changes affect which actions are allowed.
        subs.push(bus.subscribe(
            Some(LOGIND),
            Some(dbus::PROPERTIES),
            Some("PropertiesChanged"),
            Some(LOGIND_PATH),
            {
                let weak = Rc::downgrade(self);
                let bus = bus.clone();
                move |_| crate::dbus::spawn(query_actions(bus.clone(), weak.clone()))
            },
        ));

        *self.subs.borrow_mut() = subs;
    }

    fn set_battery(&self, next: BatteryState) {
        let mut cur = self.battery.borrow_mut();
        if *cur == next {
            return;
        }
        *cur = next.clone();
        drop(cur);
        debug!("battery: {}", next.label());
        self.battery_emitter.emit(&next);
    }

    fn set_profile(&self, next: ProfileState) {
        let mut cur = self.profile.borrow_mut();
        if *cur == next {
            return;
        }
        *cur = next.clone();
        drop(cur);
        debug!("power profile: {}", next.label());
        self.profile_emitter.emit(&next);
    }

    fn set_actions(&self, next: PowerActions) {
        let mut cur = self.actions.borrow_mut();
        if *cur == next {
            return;
        }
        *cur = next;
        drop(cur);
        self.actions_emitter.emit(&next);
    }

    // -- accessors ----------------------------------------------------------

    pub fn battery(&self) -> std::cell::Ref<'_, BatteryState> {
        self.battery.borrow()
    }

    pub fn on_battery_change(&self, f: impl Fn(&BatteryState) + 'static) -> Subscription {
        self.battery_emitter.subscribe(f)
    }

    pub fn profile(&self) -> std::cell::Ref<'_, ProfileState> {
        self.profile.borrow()
    }

    pub fn on_profile_change(&self, f: impl Fn(&ProfileState) + 'static) -> Subscription {
        self.profile_emitter.subscribe(f)
    }

    pub fn actions(&self) -> PowerActions {
        *self.actions.borrow()
    }

    pub fn on_actions_change(&self, f: impl Fn(&PowerActions) + 'static) -> Subscription {
        self.actions_emitter.subscribe(f)
    }

    // -- actions ------------------------------------------------------------

    /// Switch to a named power profile.
    pub fn set_profile_by_name(&self, name: &str) {
        let Some(bus) = &self.bus else { return };
        let Some((dest, path)) = self.profile_daemon.borrow().clone() else {
            warn!("no power profile daemon available");
            return;
        };
        let iface = if dest == HADESS { "net.hadess.PowerProfiles" } else { UPOWER_PROFILES };
        // `Profiles` is a{ss} on power-profiles-daemon and as on UPower; read
        // whichever this daemon uses, then set the active one.
        let bus2 = bus.clone();
        let name_owned = name.to_string();
        crate::dbus::spawn(async move {
            let ok = if dest == HADESS {
                let body = glib::Variant::tuple_from_iter([&name_owned.to_variant()]);
                bus2.call(&dest, &path, iface, "SetActiveProfile", Some(&body)).await.is_ok()
            } else {
                let body = glib::Variant::tuple_from_iter([&name_owned.to_variant()]);
                bus2.call(&dest, &path, iface, "SetProfile", Some(&body)).await.is_ok()
            };
            if !ok {
                warn!("could not switch power profile to `{name_owned}`");
            }
        });
    }

    pub fn power_off(&self) {
        self.call_logind("PowerOff");
    }

    pub fn reboot(&self) {
        self.call_logind("Reboot");
    }

    pub fn suspend(&self) {
        self.call_logind("Suspend");
    }

    fn call_logind(&self, method: &str) {
        let Some(bus) = &self.bus else { return };
        // `interactive = false`: the user already confirmed in the panel, and a
        // second polkit prompt on top of our own dialog is just noise.
        let body = glib::Variant::tuple_from_iter([&false.to_variant()]);
        bus.call_no_reply(LOGIND, LOGIND_PATH, LOGIND_MANAGER, method, Some(&body));
    }
}

async fn find_battery(bus: Bus, weak: Weak<Power>) {
    let Some(me) = weak.upgrade() else { return };
    let Ok(objects) = bus.call(UPOWER, UPOWER_PATH, dbus::OBJECT_MANAGER, "GetManagedObjects", None).await else {
        me.set_battery(BatteryState::default());
        return;
    };
    let Some(objects) = objects else {
        me.set_battery(BatteryState::default());
        return;
    };

    // Prefer the main battery; fall back to the first one that claims to be
    // present. `Battery1`/`Device` both carry the properties we need.
    let mut fallback = None;
    for entry in objects.iter() {
        let path = entry.child_value(0).get::<String>().unwrap_or_default();
        let props = dbus::variant_to_dict(entry.child_value(1)).get(UPOWER_DEVICE).cloned();
        let Some(props) = props else { continue };
        let props = dbus::variant_to_dict(props);
        let is_battery = props.get("Type").and_then(dbus::as_str).as_deref() == Some("Battery")
            || dbus::variant_to_dict(entry.child_value(1)).contains_key("org.freedesktop.UPower.Device");
        if !is_battery {
            continue;
        }
        if path.contains("BAT") || path.ends_with("battery_BAT0") {
            *me.battery_path.borrow_mut() = Some(path);
            break;
        }
        fallback.get_or_insert(path);
    }
    if me.battery_path.borrow().is_none() {
        *me.battery_path.borrow_mut() = fallback;
    }
    refresh_battery(bus, weak).await;
}

async fn refresh_battery(bus: Bus, weak: Weak<Power>) {
    let Some(me) = weak.upgrade() else { return };
    let Some(path) = me.battery_path.borrow().clone() else {
        me.set_battery(BatteryState::default());
        return;
    };
    let Ok(props) = bus.get_all(UPOWER, &path, UPOWER_DEVICE).await else {
        me.set_battery(BatteryState::default());
        return;
    };

    let present = props.get("IsPresent").and_then(dbus::as_bool).unwrap_or(false);
    let state = props.get("State").and_then(dbus::as_u32).unwrap_or(0);
    me.set_battery(BatteryState {
        present,
        // UPower reports a double here; clamp because a battery driver that
        // momentarily reports 100.4 should not round the label to 100%.
        percentage: props.get("Percentage").and_then(dbus::as_f64).unwrap_or(0.0).clamp(0.0, 100.0) as u8,
        // UPower states: 1 discharging, 2 AC, 3 fully charged, 4 charging.
        charging: state == 2 || state == 3 || state == 4,
        time_to_empty: props.get("TimeToEmpty").and_then(|v| dbus::unwrap(&v).get::<i64>()),
        warning_level: props.get("WarningLevel").and_then(dbus::as_u32).unwrap_or(0),
    });
}

async fn find_profiles(bus: Bus, weak: Weak<Power>) {
    let Some(me) = weak.upgrade() else { return };
    if !bus.name_has_owner(HADESS) {
        if !bus.name_has_owner(UPOWER_PROFILES) {
            *me.profile_daemon.borrow_mut() = None;
            me.set_profile(ProfileState::default());
            return;
        }
        *me.profile_daemon.borrow_mut() = Some((UPOWER_PROFILES.to_string(), UPOWER_PROFILES_PATH.to_string()));
    } else {
        *me.profile_daemon.borrow_mut() = Some((HADESS.to_string(), HADESS_PATH.to_string()));
    }
    refresh_profiles(bus, weak).await;
}

async fn refresh_profiles(bus: Bus, weak: Weak<Power>) {
    let Some(me) = weak.upgrade() else { return };
    let Some((dest, path)) = me.profile_daemon.borrow().clone() else { return };
    let iface = if dest == HADESS { "net.hadess.PowerProfiles" } else { UPOWER_PROFILES };

    let Ok(props) = bus.get_all(&dest, &path, iface).await else {
        me.set_profile(ProfileState::default());
        return;
    };

    // power-profiles-daemon exposes `Profiles` as a{ss} (key -> description);
    // UPower exposes it as `as`. Normalise to a stable list of labels.
    let profiles = match props.get("Profiles") {
        Some(v) => {
            let inner = dbus::unwrap(v);
            if inner.type_().is_subtype_of(glib::VariantTy::DICTIONARY) {
                // power-profiles-daemon exposes `a{ss}` (key -> description).
                let mut entries: Vec<(String, String)> = inner
                    .iter()
                    .filter_map(|e| {
                        let key = e.child_value(0).get::<String>()?;
                        let label = e.child_value(1).get::<String>().unwrap_or_else(|| key.clone());
                        Some((key, label))
                    })
                    .collect();
                // Sort by key so the tile order is stable across reads; the
                // daemon's own ordering is not guaranteed.
                entries.sort();
                entries.into_iter().map(|(key, _)| key).collect()
            } else {
                dbus::as_strv(&inner)
            }
        }
        None => Vec::new(),
    };

    me.set_profile(ProfileState {
        available: !profiles.is_empty(),
        profiles,
        active: props.get("ActiveProfile").and_then(dbus::as_str),
    });
}

async fn query_actions(bus: Bus, weak: Weak<Power>) {
    let Some(me) = weak.upgrade() else { return };

    // logind returns a *string* for CanPowerOff and friends, not a boolean:
    // "yes", "no", "challenge" or "na".
    async fn allowed(bus: &Bus, method: &str) -> bool {
        match bus.call(LOGIND, LOGIND_PATH, LOGIND_MANAGER, method, None).await {
            // "challenge" means polkit would prompt, which is still a yes for
            // our purposes.
            Ok(Some(v)) => matches!(v.child_value(0).get::<String>().as_deref(), Some("yes") | Some("challenge")),
            _ => false,
        }
    }

    let power_off = allowed(&bus, "CanPowerOff").await;
    let reboot = allowed(&bus, "CanReboot").await;
    let suspend = allowed(&bus, "CanSuspend").await;
    me.set_actions(PowerActions { can_power_off: power_off, can_reboot: reboot, can_suspend: suspend });
}

thread_local! {
    static SERVICE: RefCell<Option<Rc<Power>>> = const { RefCell::new(None) };
}

/// The process-wide power service, started on first use.
pub fn get() -> Rc<Power> {
    SERVICE.with(|slot| {
        if let Some(svc) = slot.borrow().as_ref() {
            return svc.clone();
        }
        let svc = match Bus::system() {
            Ok(bus) => Power::new(Some(bus)),
            Err(e) => {
                warn!("system bus unreachable, power features disabled: {e}");
                Power::new(None)
            }
        };
        *slot.borrow_mut() = Some(svc.clone());
        svc
    })
}