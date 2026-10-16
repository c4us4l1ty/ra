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
use crate::{debug, warn};

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
}

impl Bus {
    /// Connect to the per-user session bus.
    pub fn session() -> Result<Self> {
        let conn = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).map_err(Error::Bus)?;
        Ok(Self { conn })
    }

    /// Connect to the system bus. NetworkManager, BlueZ, UPower, logind and
    /// power-profiles all live here.
    pub fn system() -> Result<Self> {
        let conn = gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>).map_err(Error::Bus)?;
        Ok(Self { conn })
    }

    /// The underlying connection, for exporting an interface of our own.
    pub fn connection(&self) -> &gio::DBusConnection {
        &self.conn
    }

    /// Take ownership of `name`, replacing any current owner.
    ///
    /// `REPLACE` rather than `ALLOW_REPLACEMENT` plus a handshake, because a
    /// notification daemon that waits to be asked politely is a notification
    /// daemon that ends up owning nothing: the incumbent was started first and
    /// has no reason to yield. The incumbent's name-loss handler is what lets
    /// it stand down cleanly, and callers here watch that to report the
    /// situation rather than to retry.
    pub fn take_name(&self, name: &str, on_lost: impl Fn() + 'static) -> gio::OwnerId {
        gio::bus_own_name_on_connection(
            &self.conn,
            name,
            gio::BusNameOwnerFlags::REPLACE,
            |_, _| {},
            move |_, _| on_lost(),
        )
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

    /// Invoke a method synchronously.
    ///
    /// Only for one-shot CLI paths such as `ra --toggle`, where there is no
    /// main loop to run while waiting. Long-lived code must use [`Bus::call`]
    /// so the main loop is never blocked.
    pub fn call_blocking(&self, dest: &str, path: &str, iface: &str, method: &str, body: Option<&glib::Variant>) -> Result<()> {
        self.conn
            .call_sync(
                Some(dest),
                path,
                iface,
                method,
                body,
                None,
                gio::DBusCallFlags::NONE,
                -1,
                None::<&gio::Cancellable>,
            )
            .map(|_| ())
            .map_err(Error::Call)
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
            // `GetAll` is declared `-> a{sv}`, so the reply wraps the map in a
            // one-element tuple. See `reply_value`.
            Ok(variant_to_dict(reply_value(reply)))
        })
    }

    /// `org.freedesktop.DBus.Properties.Get` -> a single value, unwrapped.
    pub fn get(&self, dest: &str, path: &str, iface: &str, prop: &str) -> CallFuture<glib::Variant> {
        let body = glib::Variant::tuple_from_iter([&iface.to_variant(), &prop.to_variant()]);
        let owner = format!("{iface}.{prop}");
        let fut = self.call(dest, path, PROPERTIES, "Get", Some(&body));
        Box::pin(async move {
            let reply = fut
                .await?
                .ok_or_else(|| glib::Error::new(gio::DBusError::Failed, &format!("no value for {owner}")))?;
            // `Get` is declared `-> v`, so the reply is a one-element tuple
            // holding the value, itself wrapped in a variant. Two levels.
            Ok(unwrap(&reply_value(reply)))
        })
    }

    /// `org.freedesktop.DBus.Properties.Set`, ignoring the reply.
    pub fn set_no_reply(&self, dest: &str, path: &str, iface: &str, prop: &str, value: &glib::Variant) {
        let body = glib::Variant::tuple_from_iter([&iface.to_variant(), &prop.to_variant(), value]);
        self.call_no_reply(dest, path, PROPERTIES, "Set", Some(&body));
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
    if !v.is_container() {
        return HashMap::new();
    }
    let mut out = HashMap::with_capacity(v.n_children());
    for child in v.iter() {
        // Dict entries are two-column. Anything else - a bare array handed in
        // by mistake, an empty tuple - is skipped rather than crashing the
        // panel: a malformed reply should degrade, never take the process down.
        if !child.is_container() || child.n_children() != 2 {
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

pub fn as_u8(v: &glib::Variant) -> Option<u8> {
    unwrap(v).get::<u8>()
}

pub fn as_f64(v: &glib::Variant) -> Option<f64> {
    unwrap(v).get::<f64>()
}

/// Unwrap a single-valued method reply.
///
/// D-Bus method replies are *always* tuples. A method declared `-> ao` replies
/// with a one-element tuple **around** the array, so handing the reply straight
/// to a reader yields a tuple whose single element is the array - which parses
/// as an empty string list rather than as an error. That failure mode is
/// silent, so every reply reader goes through here.
///
/// A reply with more than one element is returned untouched, for the methods
/// that genuinely return several values.
pub fn reply_value(v: glib::Variant) -> glib::Variant {
    if v.is_container() && v.n_children() == 1 {
        v.child_value(0)
    } else {
        v
    }
}

/// Read an `as` (array of strings), accepting both a bare array and a
/// single-element tuple wrapping one.
pub fn as_strv(v: &glib::Variant) -> Vec<String> {
    let value = reply_value(unwrap(v));
    if !value.is_container() {
        return Vec::new();
    }
    value.iter().filter_map(|c| c.str().map(str::to_string)).collect()
}

/// Decode an `ay`/`az` (byte array) as UTF-8, the way NetworkManager encodes
/// Wi-Fi SSIDs.
pub fn as_bytes_str(v: &glib::Variant) -> Option<String> {
    let bytes: Vec<u8> = unwrap(v).get::<Vec<u8>>()?;
    String::from_utf8(bytes).ok()
}

/// Build an `a{sv}` from string keys and already-built values.
///
/// glib 0.22 dropped the `Variant::from_a{sv}` constructors, and the remaining
/// primitives are traps:
///
/// * `Variant::from_dict_entry` builds the *concrete* type of the entry
///   (`{sb}`, `{su}`, ...), not the fixed `{sv}` that `a{sv}` requires, so
///   combining them fails an assertion at runtime;
/// * glib-rs exposes no `g_variant_new_variant`, so a bare `v` cannot be
///   constructed to fill the entry by hand.
///
/// What does work is parsing with the container type supplied: GVariant coerces
/// each value to the declared `v` itself. The values are printed back as
/// GVariant text, which is lossless and handles escaping, so this is exact for
/// every type including byte arrays.
///
/// The round trip costs a few microseconds and happens once per network join,
/// never on a hot path.
pub fn vardict(entries: Vec<(String, glib::Variant)>) -> glib::Variant {
    let body = entries
        .iter()
        .map(|(key, value)| {
            // `print` produces properly quoted GVariant text for both halves,
            // so a key containing a quote or a backslash cannot break this.
            let key_text = glib::Variant::from(key.as_str()).print(false);
            format!("{key_text}: <{}>", value.print(false))
        })
        .collect::<Vec<_>>()
        .join(", ");

    let text = format!("@a{{sv}} {{ {body} }}");
    match glib::Variant::parse(Some(dict_type()), &text) {
        Ok(dict) => dict,
        Err(e) => {
            crate::error!("cannot build a{{sv}} ({e}); sending an empty dict instead");
            empty_dict()
        }
    }
}

/// The `a{sv}` container type.
///
/// `VariantTy::new` borrows from its argument, so passing a literal yields a
/// `&'static` type with no allocation and no interning table.
fn dict_type() -> &'static glib::VariantTy {
    glib::VariantTy::new("a{sv}").expect("`a{sv}` is a valid GVariant type")
}

/// An `a{sv}` with no entries, for calls that take an options map.
pub fn empty_dict() -> glib::Variant {
    glib::Variant::parse(Some(dict_type()), "@a{sv} {}").expect("an empty a{sv} always parses")
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
    alive: Rc<Cell<bool>>,
    handler: Handler<T>,
}

struct Registry<T> {
    slots: RefCell<Vec<Slot<T>>>,
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
        Self { registry: Rc::new(Registry { slots: RefCell::new(Vec::new()) }) }
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
        let alive = Rc::new(Cell::new(true));
        self.registry.slots.borrow_mut().push(Slot { alive: alive.clone(), handler: Box::new(handler) });
        Subscription { alive }
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
#[cfg(test)]
mod tests {
    use super::*;
    use gtk4::prelude::ToVariant;

    /// Build an `as` for the tests. Parsing is the only construction that works
    /// for a homogeneous string array in glib-rs 0.22.
    fn string_array(items: &[&str]) -> glib::Variant {
        let list = items.iter().map(|s| format!("'{}'", s.replace('\'', "\\'"))).collect::<Vec<_>>().join(", ");
        glib::Variant::parse(Some(glib::VariantTy::new("as").unwrap()), &format!("[{list}]"))
            .expect("a string array always parses")
    }

    /// A D-Bus reply is always a tuple. A method declared `-> ao` wraps the
    /// array in a one-element tuple, and reading the reply directly yields
    /// *nothing at all* rather than an error, which is the failure mode that
    /// made every list in the panel come back empty.
    #[test]
    fn reply_value_unwraps_a_single_valued_reply() {
        let array = string_array(&["/a", "/b"]);
        let reply = glib::Variant::tuple_from_iter([&array]);
        assert_eq!(reply.n_children(), 1);

        let value = reply_value(reply);
        assert_eq!(value.type_(), array.type_());
        assert_eq!(as_strv(&value), vec!["/a", "/b"]);
    }

    #[test]
    fn reply_value_leaves_multi_valued_replies_alone() {
        let reply = glib::Variant::tuple_from_iter([&"a".to_variant(), &"b".to_variant()]);
        assert_eq!(reply_value(reply).n_children(), 2);
    }

    /// A non-container must not reach `n_children()`: GVariant asserts, and a
    /// panel that aborts on a malformed reply is worse than one that degrades.
    #[test]
    fn reply_value_tolerates_a_bare_value() {
        let bare = "not a container".to_variant();
        assert_eq!(reply_value(bare.clone()), bare);
        assert_eq!(as_strv(&bare), Vec::<String>::new());
    }

    #[test]
    fn variant_to_dict_unpacks_two_column_entries() {
        let map = vardict(vec![
            ("Name".to_string(), "Built-in".to_variant()),
            ("Powered".to_string(), true.to_variant()),
        ]);
        let parsed = variant_to_dict(map);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.get("Name").and_then(as_str).as_deref(), Some("Built-in"));
        assert_eq!(parsed.get("Powered").and_then(as_bool), Some(true));
    }

    #[test]
    fn variant_to_dict_ignores_non_dict_entries() {
        // An array of plain values, handed in by mistake, must not panic.
        assert!(variant_to_dict(string_array(&["x", "y"])).is_empty());
    }

    /// UPower reports `Percentage` as a double, and NetworkManager reports
    /// `Strength` as a byte. Reading either with the wrong accessor must return
    /// `None`, never a wrong number.
    #[test]
    fn typed_accessors_reject_the_wrong_type() {
        assert_eq!(as_u8(&78.0_f64.to_variant()), None);
        assert_eq!(as_u32(&true.to_variant()), None);
        assert_eq!(as_f64(&78_u8.to_variant()), None);
        assert_eq!(as_u8(&78_u8.to_variant()), Some(78));
    }

    #[test]
    fn unwrap_strips_only_one_level_of_variant() {
        let inner = 42_u32.to_variant();
        let wrapped = glib::Variant::from_variant(&inner);
        assert!(wrapped.type_().is_variant());
        assert_eq!(unwrap(&wrapped).get::<u32>(), Some(42));

        let not_wrapped = 42_u32.to_variant();
        assert_eq!(unwrap(&not_wrapped).get::<u32>(), Some(42));
    }

    /// `/` is D-Bus's "no object" and must not become a real path.
    #[test]
    fn a_root_path_reads_as_absent() {
        assert_eq!(as_path(&"/".to_variant()), None);
        assert_eq!(as_path(&"/org/bluez/hci0".to_variant()), Some("/org/bluez/hci0".into()));
        // Not a path at all.
        assert_eq!(as_path(&"nonsense".to_variant()), None);
    }

    /// SSIDs arrive as a byte array; a non-UTF-8 one must degrade, not panic.
    #[test]
    fn byte_arrays_decode_and_survive_invalid_utf8() {
        let good = glib::Variant::from(vec![b'h', b'i']);
        assert_eq!(as_bytes_str(&good).as_deref(), Some("hi"));
        let bad = glib::Variant::from(vec![0xff_u8, 0xfe]);
        assert_eq!(as_bytes_str(&bad), None);
    }
}
