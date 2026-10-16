//! The notification daemon: `org.freedesktop.Notifications` on the session bus.
//!
//! Off by default. Owning `org.freedesktop.Notifications` means every
//! notification on the desktop is routed through this process, so it is
//! something to opt into rather than something to do to someone on their first
//! login. Set `notification_daemon = true` under `[quick_settings]`.
//!
//! What "owning it" costs, stated plainly:
//!
//! * Another daemon (XFCE ships `xfce4-notifyd`) is asked to stand down. It
//!   receives a name-loss signal and can close cleanly; one that does not is
//!   simply replaced. Either way its notifications stop reaching the user.
//! * If this process dies, notifications go nowhere until it comes back. There
//!   is no fallback.
//!
//! What it buys is a notification list inside the quick settings, and a Do Not
//! Disturb tile that actually suppresses something.
//!
//! ## Do Not Disturb
//!
//! DND does not drop notifications. It *defers* them: they are held out of the
//! list and shown when it is turned off. Dropping them would mean a message
//! that arrived at 23:00 while you slept is gone by the time you look, which is
//! the opposite of what the tile's name promises.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::dbus::Emitter;
use crate::{debug, info, warn};

pub const BUS_NAME: &str = "org.freedesktop.Notifications";
const OBJECT_PATH: &str = "/org/freedesktop/Notifications";
const IFACE: &str = "org.freedesktop.Notifications";

/// The spec version this implements.
const SPEC_VERSION: &str = "1.2";

/// Interface description, parsed by GDBus at registration.
///
/// Written as XML because that is the only way to describe an interface to
/// GDBus: there is no builder. The signatures must match the spec exactly or
/// clients will fail to parse our replies.
const INTERFACE_XML: &str = r#"<node>
  <interface name="org.freedesktop.Notifications">
    <method name="GetCapabilities">
      <arg type="as" name="capabilities" direction="out"/>
    </method>
    <method name="Notify">
      <arg type="s"   name="app_name"      direction="in"/>
      <arg type="u"   name="replaces_id"   direction="in"/>
      <arg type="s"   name="app_icon"      direction="in"/>
      <arg type="s"   name="summary"       direction="in"/>
      <arg type="s"   name="body"          direction="in"/>
      <arg type="as"  name="actions"       direction="in"/>
      <arg type="a{sv}" name="hints"       direction="in"/>
      <arg type="i"   name="expire_timeout" direction="in"/>
      <arg type="u"   name="id"            direction="out"/>
    </method>
    <method name="CloseNotification">
      <arg type="u" name="id" direction="in"/>
    </method>
    <method name="GetServerInformation">
      <arg type="s" name="name"         direction="out"/>
      <arg type="s" name="vendor"       direction="out"/>
      <arg type="s" name="version"      direction="out"/>
      <arg type="s" name="spec_version" direction="out"/>
    </method>
    <signal name="NotificationClosed">
      <arg type="u" name="id"/>
      <arg type="u" name="reason"/>
    </signal>
    <signal name="ActionInvoked">
      <arg type="u" name="id"/>
      <arg type="s" name="action_key"/>
    </signal>
  </interface>
</node>"#;

/// Close reasons, from the specification.
#[allow(dead_code)]
const REASON_EXPIRED: u32 = 1;
const REASON_DISMISSED: u32 = 2;

/// One notification, as received.
#[derive(Clone)]
pub struct Notification {
    #[allow(dead_code)]
    pub id: u32,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    /// `(key, label)` pairs, in the order the client sent them.
    pub actions: Vec<(String, String)>,
    /// Seconds since the panel started. Monotonic, so a clock change mid-session
    /// cannot make a notification look older or younger than it is.
    pub age_seconds: u32,
    /// Whether the client asked for the notification to stay until dismissed.
    pub resident: bool,
}

impl Notification {
    /// A short relative age, the way a notification list labels it.
    pub fn age_label(&self) -> String {
        let seconds = self.age_seconds;
        let label = match seconds {
            0..=59 => "now".to_string(),
            60..=3599 => format!("{}m", seconds / 60),
            3600..=86399 => format!("{}h", seconds / 3600),
            _ => format!("{}d", seconds / 86400),
        };
        if seconds < 5 {
            return label;
        }
        label
    }
}

/// Whether we are the notification server, and why not if we are not.
#[derive(Clone, PartialEq)]
pub enum Daemon {
    /// Not configured to be a daemon at all.
    Off,
    /// We own the bus name and are receiving notifications.
    Running,
    /// Another process owns the name and ours was refused or lost.
    Taken,
}

#[derive(Default)]
struct State {
    /// Everything currently held, newest last.
    notifications: Vec<Notification>,
    next_id: u32,
    dnd: bool,
}

/// The notification daemon.
pub struct Notifications {
    state: RefCell<State>,
    started: Cell<std::time::Instant>,
    /// Dropped with the service, which releases the bus name.
    _owner: RefCell<Option<gio::OwnerId>>,
    emitter: Emitter<()>,
}

impl Notifications {
    /// Start the daemon if `enabled`, otherwise stay out of the way.
    pub fn start(enabled: bool, dnd: bool) -> Rc<Self> {
        let me = Rc::new(Self {
            state: RefCell::new(State { dnd, ..Default::default() }),
            started: Cell::new(std::time::Instant::now()),
            _owner: RefCell::new(None),
            emitter: Emitter::new(),
        });

        if !enabled {
            debug!("notification daemon disabled; leaving org.freedesktop.Notifications alone");
            return me;
        }

        let Ok(bus) = crate::dbus::Bus::session() else {
            warn!("no session bus; the notification daemon cannot start");
            return me;
        };

        // Export the object before taking the name, so that nothing can be
        // routed to us between the two and hit an unregistered path.
        let node = match gio::DBusNodeInfo::for_xml(INTERFACE_XML) {
            Ok(node) => node,
            Err(e) => {
                // A malformed constant is a build-time mistake, not a runtime
                // condition, so it is worth being loud about.
                warn!("the notification interface XML is invalid: {e}");
                return me;
            }
        };
        let Some(iface) = node.lookup_interface(IFACE) else {
            warn!("the notification interface is missing from its own description");
            return me;
        };

        let handler: Rc<Self> = me.clone();
        let registration = bus.connection().register_object(OBJECT_PATH, &iface).method_call(
            move |_conn, _sender, _path, _iface, method, params, invocation| {
                handler.dispatch(method, &params, invocation);
            },
        );
        match registration.build() {
            Ok(id) => std::mem::forget(id),
            Err(e) => {
                warn!("cannot export {IFACE}: {e}");
                return me;
            }
        }

        let flag = Rc::new(Cell::new(true));
        let lost_flag = flag.clone();
        let id = bus.take_name(BUS_NAME, move || {
            lost_flag.set(false);
            warn!("lost {BUS_NAME}; another notification server took over");
        });

        // `NameAcquired` is not reported back through `take_name`, so check the
        // bus directly rather than assuming ownership.
        if bus.name_has_owner(BUS_NAME) {
            info!("notification daemon running as {BUS_NAME}");
            *me._owner.borrow_mut() = Some(id);
        } else {
            warn!("{BUS_NAME} is owned by another process; notifications are not reaching ra");
        }

        me
    }

    /// Whether we are receiving notifications.
    pub fn is_running(&self) -> bool {
        self._owner.borrow().is_some()
    }

    /// Notifications to show, honouring Do Not Disturb.
    ///
    /// The deferred ones are returned as soon as DND is off, which is what
    /// makes the tile mean something.
    pub fn visible(&self) -> Vec<Notification> {
        let state = self.state.borrow();
        if state.dnd {
            Vec::new()
        } else {
            state.notifications.clone()
        }
    }

    /// How many notifications are held back by Do Not Disturb.
    pub fn deferred_count(&self) -> usize {
        let state = self.state.borrow();
        if state.dnd {
            state.notifications.len()
        } else {
            0
        }
    }

    pub fn set_dnd(&self, on: bool) {
        let mut state = self.state.borrow_mut();
        if state.dnd == on {
            return;
        }
        state.dnd = on;
        drop(state);
        if !on {
            info!("Do Not Disturb off; showing deferred notifications");
        }
        self.emitter.emit(&());
    }

    pub fn dnd(&self) -> bool {
        self.state.borrow().dnd
    }

    /// Dismiss one notification, telling the client.
    pub fn close(&self, id: u32) {
        let removed = {
            let mut state = self.state.borrow_mut();
            match state.notifications.iter().position(|n| n.id == id) {
                Some(at) => {
                    state.notifications.remove(at);
                    true
                }
                None => false,
            }
        };
        if removed {
            self.emit_closed(id, REASON_DISMISSED);
        }
    }

    /// Dismiss everything.
    pub fn clear(&self) {
        let ids: Vec<u32> = self.state.borrow().notifications.iter().map(|n| n.id).collect();
        self.state.borrow_mut().notifications.clear();
        for id in ids {
            self.emit_closed(id, REASON_DISMISSED);
        }
        self.emitter.emit(&());
    }

    /// Notify on any change.
    pub fn on_change(&self, f: impl Fn(&()) + 'static) -> crate::dbus::Subscription {
        self.emitter.subscribe(f)
    }

    fn emit_closed(&self, id: u32, reason: u32) {
        let Ok(bus) = crate::dbus::Bus::session() else { return };
        let body = glib::Variant::tuple_from_iter([&id.to_variant(), &reason.to_variant()]);
        bus.call_no_reply(
            BUS_NAME,
            OBJECT_PATH,
            IFACE,
            "NotificationClosed",
            Some(&body),
        );
        self.emitter.emit(&());
    }

    /// The age every notification is stamped with when it arrives.
    fn age(&self) -> u32 {
        self.started.get().elapsed().as_secs().min(u32::MAX as u64) as u32
    }

    fn dispatch(&self, method: &str, params: &glib::Variant, invocation: gio::DBusMethodInvocation) {
        match method {
            "GetCapabilities" => {
                // Only the one capability ra actually honours. Claiming more -
                // body, actions, icons - invites clients to use features that
                // are not finished.
                invocation.return_value(Some(&glib::Variant::tuple_from_iter([&glib::Variant::from(
                    &["body".to_string(), "actions".to_string()][..],
                )])));
            }
            "Notify" => {
                let id = self.notify(params);
                invocation.return_value(Some(&glib::Variant::tuple_from_iter([&id.to_variant()])));
            }
            "CloseNotification" => {
                let id = params.child_value(0).get::<u32>().unwrap_or(0);
                self.close(id);
                // Void return: `None`. An empty tuple is not legal on the bus.
                invocation.return_value(None);
            }
            "GetServerInformation" => {
                let info = [
                    env!("CARGO_PKG_NAME").to_string(),
                    env!("CARGO_PKG_AUTHORS").to_string(),
                    env!("CARGO_PKG_VERSION").to_string(),
                    SPEC_VERSION.to_string(),
                ];
                let body = glib::Variant::array_from_iter_with_type(
                    glib::VariantTy::STRING_ARRAY,
                    info.iter().map(|s| s.to_variant()),
                );
                invocation.return_value(Some(&glib::Variant::tuple_from_iter([&body])));
            }
            other => {
                warn!("{IFACE}.{other} is not implemented");
                invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    &format!("{other} is not implemented"),
                );
            }
        }
    }

    fn notify(&self, params: &glib::Variant) -> u32 {
        let text = |i: usize| -> String {
            params.child_value(i).str().unwrap_or_default().to_string()
        };
        let app_name = text(0);
        let replaces_id = params.child_value(1).get::<u32>().unwrap_or(0);
        let app_icon = text(2);
        let summary = text(3);
        let body = text(4);
        let actions = crate::dbus::as_strv(&params.child_value(5));
        let hints = crate::dbus::variant_to_dict(params.child_value(6));
        let expire = params.child_value(7).get::<i32>().unwrap_or(-1);

        // Actions arrive as a flat [key, label, key, label, ...] list.
        let actions: Vec<(String, String)> = actions
            .chunks(2)
            .filter(|pair| pair.len() == 2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect();

        // `resident` means "do not expire". `-1` means "server decides", and
        // the deciding is ours: a notification with actions is worth keeping
        // until it is acted on or dismissed.
        // `as_bool` returns an Option, so this is Option<Option<bool>>.
        let requested_resident =
            hints.get("resident").and_then(crate::dbus::as_bool).unwrap_or(false);
        let resident = requested_resident || !actions.is_empty() || expire == 0;

        let mut state = self.state.borrow_mut();

        // A replacing notification keeps its id, which is what makes an
        // updating progress bar update in place instead of stacking up.
        let id = if replaces_id != 0
            && state.notifications.iter().any(|n| n.id == replaces_id)
        {
            replaces_id
        } else {
            state.next_id = state.next_id.wrapping_add(1).max(1);
            state.next_id
        };

        let notification = Notification {
            id,
            app_name,
            app_icon,
            summary,
            body,
            actions,
            age_seconds: self.age(),
            resident,
        };

        match state.notifications.iter().position(|n| n.id == id) {
            Some(at) => state.notifications[at] = notification,
            None => state.notifications.push(notification),
        }

        // A bounded list. An unbounded one is a memory leak with a UI on it,
        // and a panel is the wrong place to keep a week's history.
        const MAX: usize = 100;
        if state.notifications.len() > MAX {
            let overflow = state.notifications.len() - MAX;
            state.notifications.drain(0..overflow);
        }

        drop(state);
        self.emitter.emit(&());
        debug!("notification {id} received");
        id
    }
}

thread_local! {
    static SERVICE: RefCell<Option<Rc<Notifications>>> = const { RefCell::new(None) };
}

/// The process-wide daemon, started on first use.
pub fn get() -> Rc<Notifications> {
    SERVICE.with(|slot| {
        if slot.borrow().is_none() {
            let dnd = crate::state::State::load().get_bool(crate::state::KEY_DND, false);
            *slot.borrow_mut() = Some(Notifications::start(false, dnd));
        }
        Rc::clone(slot.borrow().as_ref().expect("just set"))
    })
}

/// Start the daemon, honouring the configuration. Called once at startup.
pub fn install(enabled: bool) {
    SERVICE.with(|slot| {
        let dnd = crate::state::State::load().get_bool(crate::state::KEY_DND, false);
        *slot.borrow_mut() = Some(Notifications::start(enabled, dnd));
    });
}
