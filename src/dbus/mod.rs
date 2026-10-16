//! A thin wrapper around GDBus (`gio`), which ships with GLib and is therefore
//! already linked into the binary by `gtk4`.
//!
//! Why not `zbus` or `dbus-rs`? Both would add a dozen crates, and `zbus`
//! would drag an async executor and extra threads into the process. The whole
//! panel runs on the GLib main loop anyway, so the honest choice is to use the
//! bindings we already pay for.
//!
//! Every call is non-blocking: the returned future completes only when the main
//! loop dispatches the reply, so an idle panel simply parks in `poll()`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::future::oneshot;
use crate::{debug, error, warn};

/// Errors that can happen while talking to D-Bus.
#[derive(Debug)]
pub enum Error {
    /// The bus itself could not be reached (usually the system bus).
    Bus(glib::Error),
    /// The method call failed or returned an unexpected reply.
    Call(glib::Error),
    /// The reply arrived after the oneshot channel was dropped.
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bus(e) => write!(f, "cannot reach bus: {e}"),
            Self::Call(e) => write!(f, "call failed: {e}"),
            Self::Cancelled => f.write_str("call cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<glib::Error> for Error {
    fn from(e: glib::Error) -> Self {
        Self::Call(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Boxed future returned by the async call helpers.
pub type CallFuture<T> = Pin<Box<dyn Future<Output = Result<T>>>>;

pub const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
pub const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
pub const DBUS_IFACE: &str = "org.freedesktop.DBus";
pub const DBUS_PATH: &str = "/org/freedesktop/DBus";

/// A connection to one of the two message buses.
///
/// GDBus connections are reference-counted and shared process-wide, so
/// creating one per call is cheap and callers are free to hold their own.
#[derive(Clone)]
pub struct Bus {
    conn: gio::DBusConnection,
    system: bool,
}

impl Bus {
    /// Connect to the per-user session bus.
    pub fn session() -> Result<Self> {
        let conn = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).map_err(Error::Bus)?;
        Ok(Self { conn, system: false })
    }

    /// Connect to the system bus. NetworkManager, BlueZ, UPower, logind and
    /// power-profiles all live here.
    pub fn system() -> Result<Self> {
        let conn = gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>).map_err(Error::Bus)?;
        Ok(Self { conn, system: true })
    }

    pub fn is_system(&self) -> bool {
        self.system
    }

    pub fn connection(&self) -> &gio::DBusConnection {
        &self.conn
    }

    /// True when `name` currently has an owner on this bus.
    pub fn name_has_owner(&self, name: &str) -> bool {
        self.conn
            .call_sync(
                Some(DBUS_IFACE),
                DBUS_PATH,
                DBUS_IFACE,
                "NameHasOwner",
                Some(&glib::Variant::tuple_from_iter([&name.to_variant()])),
                None,
                gio::DBusCallFlags::NONE,
                -1,
                None::<&gio::Cancellable>,
            )
            .ok()
            .and_then(|v| v.child_value(0).get::<bool>())
            .unwrap_or(false)
    }

    /// Every well-known name on the bus. Used for feature detection and shown
    /// by `ra --diagnose`.
    pub fn list_names(&self) -> Vec<String> {
        self.conn
            .call_sync(
                Some(DBUS_IFACE),
                DBUS_PATH,
                DBUS_IFACE,
                "ListNames",
                None,
                None,
                gio::DBusCallFlags::NONE,
                -1,
                None::<&gio::Cancellable>,
            )
            .ok()
            .map(|v| as_strv(&v))
            .unwrap_or_default()
    }

    /// Invoke a method and return its reply body.
    pub fn call(
        &self,
        dest: &str,
        path: &str,
        iface: &str,
        method: &str,
        body: Option<&glib::Variant>,
    ) -> CallFuture<Option<glib::Variant>> {
        let conn = self.conn.clone();
        let (dest, path, iface, method) =
            (dest.to_string(), path.to_string(), iface.to_string(), method.to_string());
        let body = body.cloned();
        Box::pin(async move {
            let (tx, rx) = oneshot::<std::result::Result<glib::Variant, glib::Error>>();
            conn.call(
                Some(&dest),
                &path,
                &iface,
                &method,
                body.as_ref(),
                None,
                gio::DBusCallFlags::NONE,
                -1,
                None::<&gio::Cancellable>,
                move |res| {
                    let _ = tx.send(res);
                },
            );
            match rx.await {
                Err(_) => Err(Error::Cancelled),
                Ok(Err(e)) => {
                    // A service that simply is not installed is an expected
                    // state, not a panel bug; the caller decides how loud to be.
                    debug_dbus_error(&dest, &path, &method, &e);
                    Err(Error::Call(e))
                }
                Ok(Ok(v)) => Ok(Some(v)),
            }
        })
    }

    /// Invoke a method and ignore the reply. Fire-and-forget for things like
    /// toggles, where we rely on the resulting `PropertiesChanged` to update
    /// the UI rather than on the return value.
    pub fn call_no_reply(&self, dest: &str, path: &str, iface: &str, method: &str, body: Option<&glib::Variant>) {
        let conn = self.conn.clone();
        let body = body.cloned();
        let label = format!("{iface}.{method} on {path}");
        let (dest, path, iface, method) =
            (dest.to_string(), path.to_string(), iface.to_string(), method.to_string());
        glib::idle_add_local_once(move || {
            let (label, dest, path, iface, method) = (label.clone(), dest.clone(), path.clone(), iface.clone(), method.clone());
            conn.call(
                Some(&dest),
                &path,
                &iface,
                &method,
                body.as_ref(),
                None,
                gio::DBusCallFlags::NONE,
                -1,
                None::<&gio::Cancellable>,
                move |res| {
                    if let Err(e) = res {
                        if !e.matches(gio::DBusError::Disconnected) {
                            warn!("{label}: {e}");
                        }
                    }
                },
            );
        });
    }

    /// `org.freedesktop.DBus.Properties.GetAll` -> a map of property name to value.
    pub fn get_all(&self, dest: &str, path: &str, iface: &str) -> CallFuture<HashMap<String, glib::Variant>> {
        let body = glib::Variant::tuple_from_iter([&iface.to_variant()]);
        let fut = self.call(dest, path, PROPERTIES, "GetAll", Some(&body));
        Box::pin(async move {
            let reply = fut
                .await?
                .ok_or_else(|| glib::Error::new(gio::DBusError::Failed, "GetAll returned no reply"))?;
            Ok(variant_to_dict(reply))
        })
    }

    /// `org.freedesktop.DBus.Properties.Get` -> a single (unwrapped) value.
    pub fn get(&self, dest: &str, path: &str, iface: &str, prop: &str) -> CallFuture<glib::Variant> {
        let body = glib::Variant::tuple_from_iter([&iface.to_variant(), &prop.to_variant()]);
        let owner = format!("{iface}.{prop}");
        let fut = self.call(dest, path, PROPERTIES, "Get", Some(&body));
        Box::pin(async move {
            let reply = fut
                .await?
                .ok_or_else(|| glib::Error::new(gio::DBusError::Failed, &format!("no value for {owner}")))?;
            // `Get` returns a variant-of-variant; `unwrap` strips one level.
            Ok(unwrap(&reply))
        })
    }

    /// `org.freedesktop.DBus.Properties.Set`.
    pub fn set(&self, dest: &str, path: &str, iface: &str, prop: &str, value: &glib::Variant) -> CallFuture<()> {
        let body = glib::Variant::tuple_from_iter([&iface.to_variant(), &prop.to_variant(), value]);
        let fut = self.call(dest, path, PROPERTIES, "Set", Some(&body));
        Box::pin(async move {
            fut.await?;
            Ok(())
        })
    }

    /// Subscribe to a signal. `None` in any matching position matches anything.
    /// The returned subscription unsubscribes itself when dropped.
    pub fn subscribe<F>(&self, sender: Option<&str>, iface: Option<&str>, member: Option<&str>, path: Option<&str>, f: F) -> gio::SignalSubscription
    where
        F: Fn(gio::DBusSignalRef<'_>) + 'static,
    {
        self.conn.subscribe_to_signal(
            sender,
            iface,
            member,
            path,
            None,
            gio::DBusSignalFlags::NONE,
            f,
        )
    }

    /// Watch a well-known name appear or disappear. `f(true)` means it is now
    /// owned. Services use this to survive a daemon restart without polling.
    pub fn watch_name<F>(&self, name: &str, f: F) -> gio::SignalSubscription
    where
        F: Fn(bool) + 'static,
    {
        let name = name.to_string();
        self.subscribe(
            Some(DBUS_IFACE),
            Some(DBUS_IFACE),
            Some("NameOwnerChanged"),
            Some(DBUS_PATH),
            move |sig| {
                // args: name, old_owner, new_owner
                if sig.parameters.child_value(0).get::<String>().as_deref() == Some(name.as_str()) {
                    let owned = sig.parameters.child_value(2).get::<String>().is_some();
                    f(owned);
                }
            },
        )
    }

    /// Log the reply for a failed call, distinguishing "not installed" from a
    /// genuine error so the log stays readable.
    pub fn explain(&self, what: &str, e: &Error) {
        error!("{what}: {e}");
    }
}

fn debug_dbus_error(dest: &str, path: &str, method: &str, e: &glib::Error) {
    // `ServiceUnknown` just means the daemon is not installed or not running;
    // callers handle that as a first-class "feature unavailable" state.
    let benign = e.matches(gio::DBusError::ServiceUnknown)
        || e.matches(gio::DBusError::NameHasNoOwner)
        || e.matches(gio::DBusError::UnknownObject)
        || e.matches(gio::DBusError::UnknownInterface)
        || e.matches(gio::DBusError::UnknownMethod)
        || e.matches(gio::DBusError::Disconnected);
    if benign {
        debug!("{dest}{path}.{method}: {e}");
    } else {
        warn!("{dest}{path}.{method}: {e}");
    }
}

// ---------------------------------------------------------------------------
// Variant helpers
// ---------------------------------------------------------------------------

/// Convert an `a{...}`-shaped variant into a plain map. Used for both `a{sv}`
/// property bags and `a{oa{sa{sv}}}` ObjectManager payloads (the values stay
/// variants, only the two-column entries are unpacked).
pub fn variant_to_dict(v: glib::Variant) -> HashMap<String, glib::Variant> {
    let mut out = HashMap::with_capacity(v.n_children());
    for child in v.iter() {
        if child.n_children() != 2 {
            continue;
        }
        if let Some(key) = child.child_value(0).get::<String>() {
            out.insert(key, child.child_value(1));
        }
    }
    out
}

/// Unwrap a variant-of-variant. Values arriving through `Get` are wrapped one
/// level deep; values from `GetAll` are not.
pub fn unwrap(v: &glib::Variant) -> glib::Variant {
    if v.type_().is_variant() {
        v.child_value(0)
    } else {
        v.clone()
    }
}

pub fn as_bool(v: &glib::Variant) -> Option<bool> {
    unwrap(v).get::<bool>()
}

pub fn as_str(v: &glib::Variant) -> Option<String> {
    unwrap(v).get::<String>()
}

/// Read a D-Bus object path as a plain `String`. `"/"` means "none".
///
/// D-Bus object paths have no dedicated Rust wrapper in glib; they travel as
/// plain strings that GVariant keeps typed as `o`, so reading `String` here is
/// already validated against the wire type.
pub fn as_path(v: &glib::Variant) -> Option<String> {
    let p = unwrap(v).get::<String>()?;
    if p == "/" || !p.starts_with('/') {
        None
    } else {
        Some(p)
    }
}

pub fn as_u32(v: &glib::Variant) -> Option<u32> {
    unwrap(v).get::<u32>()
}

pub fn as_i32(v: &glib::Variant) -> Option<i32> {
    unwrap(v).get::<i32>()
}

pub fn as_u8(v: &glib::Variant) -> Option<u8> {
    unwrap(v).get::<u8>()
}

pub fn as_f64(v: &glib::Variant) -> Option<f64> {
    unwrap(v).get::<f64>()
}

/// Read an `as` (array of strings).
pub fn as_strv(v: &glib::Variant) -> Vec<String> {
    unwrap(v).iter().filter_map(|c| c.str().map(str::to_string)).collect()
}

/// Decode an `ay`/`az` (byte array) as UTF-8, the way NetworkManager encodes
/// Wi-Fi SSIDs.
pub fn as_bytes_str(v: &glib::Variant) -> Option<String> {
    let bytes: Vec<u8> = unwrap(v).get::<Vec<u8>>()?;
    String::from_utf8(bytes).ok()
}

/// Build an `a{sv}` from string keys and already-built variants.
///
/// glib 0.22 dropped the `Variant::from_a{sv}` convenience constructors, so
/// `a{sv}` has to be assembled from `{sv}` dict entries.
pub fn vardict(entries: Vec<(String, glib::Variant)>) -> glib::Variant {
    use glib::variant::DictEntry;
    glib::Variant::array_from_iter::<DictEntry<String, glib::Variant>>(
        entries.into_iter().map(|(k, v)| glib::Variant::from_dict_entry(&k.to_variant(), &v)),
    )
}

/// Spawn a future on the GLib main context without pulling in an executor.
pub fn spawn<F>(future: F)
where
    F: Future<Output = ()> + 'static,
{
    glib::spawn_future_local(future);
}

// ---------------------------------------------------------------------------
// A minimal pub/sub that decouples services from the widgets displaying them.
// ---------------------------------------------------------------------------

type Handler<T> = Box<dyn Fn(&T)>;

/// One subscriber. `alive` is shared with the caller's [`Subscription`], so
/// dropping the subscription detaches the handler without needing to mutate
/// the handler list while it is being iterated.
struct Slot<T> {
    id: u64,
    alive: Rc<Cell<bool>>,
    handler: Handler<T>,
}

struct Registry<T> {
    slots: RefCell<Vec<Slot<T>>>,
    next_id: Cell<u64>,
}

/// A signal source that services emit state through and widgets subscribe to.
pub struct Emitter<T> {
    registry: Rc<Registry<T>>,
}

/// Keeps a subscription alive. Dropping it unsubscribes.
pub struct Subscription {
    alive: Rc<Cell<bool>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.alive.set(false);
    }
}

impl<T: 'static> Emitter<T> {
    pub fn new() -> Self {
        Self { registry: Rc::new(Registry { slots: RefCell::new(Vec::new()), next_id: Cell::new(0) }) }
    }

    pub fn emit(&self, value: &T) {
        // Move the list out for the duration of the call so a handler may
        // freely subscribe, unsubscribe or rebuild widgets. Handlers that were
        // detached mid-emit are dropped rather than called.
        let snapshot = {
            let mut slots = self.registry.slots.borrow_mut();
            if slots.is_empty() {
                return;
            }
            std::mem::take(&mut *slots)
        };
        for slot in &snapshot {
            if slot.alive.get() {
                (slot.handler)(value);
            }
        }
        self.registry.slots.borrow_mut().extend(snapshot);
    }

    pub fn subscribe<F>(&self, handler: F) -> Subscription
    where
        F: Fn(&T) + 'static,
    {
        let id = self.registry.next_id.get().wrapping_add(1);
        self.registry.next_id.set(id);
        let alive = Rc::new(Cell::new(true));
        self.registry.slots.borrow_mut().push(Slot { id, alive: alive.clone(), handler: Box::new(handler) });
        Subscription { alive }
    }

    /// Number of live subscribers. Used by tests to prove that tearing a panel
    /// down actually releases its handlers.
    pub fn subscriber_count(&self) -> usize {
        self.registry.slots.borrow().iter().filter(|s| s.alive.get()).count()
    }
}

impl<T> Clone for Emitter<T> {
    fn clone(&self) -> Self {
        Self { registry: self.registry.clone() }
    }
}

impl<T: 'static> Default for Emitter<T> {
    fn default() -> Self {
        Self::new()
    }
}