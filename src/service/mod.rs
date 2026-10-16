//! System services.
//!
//! Every service is a lazily-started process-wide singleton: a panel that does
//! not configure the `bluetooth` module never touches BlueZ, and one that does
//! not configure `brightness` never creates a timer. Services push changes to
//! widgets through [`crate::dbus::Emitter`] rather than being polled by them.

pub mod audio;
pub mod bluetooth;
pub mod brightness;
pub mod network;
pub mod nightlight;
pub mod power;

/// Start every service, so `availability()` has something to report.
///
/// Only used by `--diagnose`, which has no config to warm services from. The
/// panel itself starts services lazily, so an unused feature costs nothing.
pub fn warm() {
    network::get();
    bluetooth::get();
    audio::get();
    brightness::get();
    power::get();
}

/// Human-readable availability of every service, for `ra --diagnose`.
pub fn availability() -> Vec<(&'static str, &'static str)> {
    let net = network::get();
    let bt = bluetooth::get();
    let power = power::get();
    let aud = audio::get();
    let bri = brightness::get();

    let net_ok = net.state().available;
    let bt_ok = bt.state().available;
    let bat_ok = power.battery().present;
    let aud_ok = aud.state().available;
    let bri_ok = bri.state().available;
    let profiles_ok = power.profile().available;

    vec![
        ("NetworkManager", if net_ok { "yes" } else { "no" }),
        ("BlueZ", if bt_ok { "yes" } else { "no" }),
        ("UPower", if bat_ok { "yes" } else { "n/a" }),
        ("WirePlumber", if aud_ok { "yes" } else { "no" }),
        ("backlight", if bri_ok { "yes" } else { "no" }),
        ("power profiles", if profiles_ok { "yes" } else { "no" }),
    ]
}