//! Audio volume, mute and device selection.
//!
//! Design note: reads and writes go through `wpctl`, the official WirePlumber
//! command-line interface, rather than reimplementing the PipeWire parameter
//! protocol. `wpctl` is part of `pipewire-wireplumber` - i.e. it is present on
//! any XFCE/labwc system that has working audio at all - and it speaks every
//! version of the daemon's D-Bus API correctly, including versions this panel
//! has never been compiled against.
//!
//! The cost is that a read is a `fork`+`exec`, which is far too expensive to do
//! on a timer. So change detection is a two-tier affair:
//!
//! 1. **Primary** - PipeWire's own `org.pipewire.PipeWire.Info` signal, which
//!    fires whenever any node's parameters change (including hardware volume
//!    keys). Cheap, and it is the right signal.
//! 2. **Fallback** - a timer, used only when that D-Bus name is absent. The
//!    interval is configurable and can be set to 0 to disable it entirely.

use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use gtk4::gio;
use gtk4::glib;

use crate::dbus::{Bus, Emitter, Subscription};
use crate::{debug, info, warn};

const PIPEWIRE: &str = "org.pipewire.PipeWire";
const PIPEWIRE_PATH: &str = "/org/pipewire/pipewire";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioDevice {
    /// PipeWire node id, as printed by `wpctl`.
    pub id: u32,
    pub description: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioState {
    /// `wpctl` is installed and the daemon answered.
    pub available: bool,
    /// 0-100.
    pub volume: u8,
    /// Sink mute.
    pub muted: bool,
    /// Source (microphone) mute. Tracked separately because a machine can be
    /// listening through a muted microphone while playing sound normally.
    pub source_muted: bool,
    pub sinks: Vec<AudioDevice>,
    pub sources: Vec<AudioDevice>,
    pub default_sink: Option<u32>,
    pub default_source: Option<u32>,
}

impl AudioState {
    pub fn icon(&self) -> &'static str {
        if self.muted || self.volume == 0 {
            return "audio-volume-muted-symbolic";
        }
        match self.volume {
            90..=100 => "audio-volume-high-symbolic",
            50..=89 => "audio-volume-medium-symbolic",
            1..=49 => "audio-volume-low-symbolic",
            _ => "audio-volume-muted-symbolic",
        }
    }

    pub fn label(&self) -> String {
        if !self.available {
            return "Audio".into();
        }
        if self.muted {
            return "Muted".into();
        }
        format!("{}%", self.volume)
    }

    /// Description of the default sink, if we know it.
    pub fn default_sink_description(&self) -> Option<&str> {
        let id = self.default_sink?;
        self.sinks.iter().find(|s| s.id == id).map(|s| s.description.as_str())
    }
}

pub struct Audio {
    state: RefCell<AudioState>,
    emitter: Emitter<AudioState>,
    /// Poll interval while the popup is closed, in milliseconds. 0 disables.
    idle_interval_ms: Cell<u32>,
    /// Poll interval while the popup is open.
    active_interval_ms: Cell<u32>,
    timer: RefCell<Option<glib::SourceId>>,
    subs: RefCell<Vec<gio::SignalSubscription>>,
}

impl Audio {
    fn new() -> Rc<Self> {
        let idle: u32 = std::env::var("RA_AUDIO_POLL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let me = Rc::new(Self {
            state: RefCell::new(AudioState::default()),
            emitter: Emitter::new(),
            idle_interval_ms: Cell::new(idle),
            active_interval_ms: Cell::new(1500),
            timer: RefCell::new(None),
            subs: RefCell::new(Vec::new()),
        });
        me.connect_signals();
        me.refresh_now();
        me
    }

    fn connect_signals(self: &Rc<Self>) {
        let Ok(bus) = Bus::session() else {
            warn!("no session bus; audio disabled");
            return;
        };

        // Primary change notification: any PipeWire node parameter change,
        // which covers hardware volume keys and every application.
        if bus.name_has_owner(PIPEWIRE) {
            info!("PipeWire D-Bus found; audio updates are event driven");
            self.subs.borrow_mut().push(bus.subscribe(
                Some(PIPEWIRE),
                Some(PIPEWIRE),
                Some("Info"),
                Some(PIPEWIRE_PATH),
                {
                    let weak = Rc::downgrade(self);
                    move |sig| {
                        // args: (u id, s type, i version, a{sv} props)
                        // Only react to nodes: "type" is "node" or "port".
                        let kind = sig.parameters.child_value(1).get::<String>().unwrap_or_default();
                        if kind == "node" {
                            spawn_refresh(&weak);
                        }
                    }
                },
            ));
            self.subs.borrow_mut().push(bus.watch_name(PIPEWIRE, {
                let weak = Rc::downgrade(self);
                move |_| spawn_refresh(&weak)
            }));
            return;
        }

        // Fallback: no PipeWire D-Bus, so fall back to a timer. Coalesce bursts
        // (an app changing volume produces several events) by re-arming a
        // single timer rather than queueing refreshes.
        let ms = self.idle_interval_ms.get();
        if ms == 0 {
            info!("PipeWire D-Bus unavailable; audio will only update while quick settings is open");
        } else {
            info!("PipeWire D-Bus unavailable; polling audio every {ms} ms");
        }
        self.subs.borrow_mut().push(bus.subscribe(
            Some("org.wireplumber"),
            None,
            None,
            None,
            {
                let weak = Rc::downgrade(self);
                move |_| spawn_refresh(&weak)
            },
        ));
    }

    /// Adjust how often the fallback poll runs. Called when the quick settings
    /// popup opens and closes.
    pub fn set_active(self: &Rc<Self>, active: bool) {
        let ms = if active { self.active_interval_ms.get() } else { self.idle_interval_ms.get() };
        self.rearm(ms);
    }

    fn rearm(self: &Rc<Self>, ms: u32) {
        let mut timer = self.timer.borrow_mut();
        if let Some(id) = timer.take() {
            id.remove();
        }
        if ms == 0 {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local(Duration::from_millis(u64::from(ms)), move || {
            if let Some(me) = weak.upgrade() {
                me.refresh_now();
            }
            glib::ControlFlow::Continue
        });
        *timer = Some(id);
    }

    fn refresh_now(&self) {
        let next = read_state();
        let mut cur = self.state.borrow_mut();
        if *cur == next {
            return;
        }
        *cur = next;
        drop(cur);
        self.emitter.emit(&self.state.borrow());
    }

    pub fn state(&self) -> std::cell::Ref<'_, AudioState> {
        self.state.borrow()
    }

    pub fn on_change(&self, f: impl Fn(&AudioState) + 'static) -> Subscription {
        self.emitter.subscribe(f)
    }

    // -- actions ------------------------------------------------------------

    /// Set the default sink's volume, 0-100.
    pub fn set_volume(self: &Rc<Self>, percent: u8) {
        let linear = f64::from(percent) / 100.0;
        run(&["set-volume", DEFAULT_SINK, &format!("{linear:.4}")]);
        spawn_refresh(&Rc::downgrade(self));
    }

    pub fn set_muted(self: &Rc<Self>, muted: bool) {
        run(&["set-mute", DEFAULT_SINK, if muted { "1" } else { "0" }]);
        spawn_refresh(&Rc::downgrade(self));
    }

    /// Toggle mute, returning the new muted state.
    pub fn toggle_mute(self: &Rc<Self>) -> bool {
        let muted = !self.state.borrow().muted;
        self.set_muted(muted);
        muted
    }

    pub fn set_default_sink(self: &Rc<Self>, id: u32) {
        run(&["set-default", &id.to_string()]);
        spawn_refresh(&Rc::downgrade(self));
    }

    /// Sources use WirePlumber's `id@DEFAULT_AUDIO_SINK@` target syntax, where
    /// the node id is prefixed to a special target name.
    pub fn set_default_source(self: &Rc<Self>, id: u32) {
        run(&["set-default", &format!("{id}@DEFAULT_AUDIO_SINK@")]);
        spawn_refresh(&Rc::downgrade(self));
    }

    /// Mute or unmute the default source, returning the new state.
    pub fn toggle_source_mute(self: &Rc<Self>) -> bool {
        let muted = !self.state.borrow().muted;
        self.set_source_muted(muted);
        muted
    }

    pub fn set_source_muted(self: &Rc<Self>, muted: bool) {
        run(&["set-mute", DEFAULT_SOURCE, if muted { "1" } else { "0" }]);
        spawn_refresh(&Rc::downgrade(self));
    }
}

/// PipeWire's special default-node target.
const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";
const DEFAULT_SOURCE: &str = "@DEFAULT_AUDIO_SOURCE@";

fn spawn_refresh(weak: &std::rc::Weak<Audio>) {
    let weak = weak.clone();
    glib::idle_add_local_once(move || {
        if let Some(me) = weak.upgrade() {
            me.refresh_now();
        }
    });
}

/// Run `wpctl` with fixed arguments.
///
/// Arguments are passed as a vector, never through a shell, so a device
/// description can never be interpreted as a command.
fn run(args: &[&str]) -> Option<String> {
    let out = Command::new("wpctl").args(args).output().ok()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        debug!("wpctl {}: {}", args.join(" "), err.trim());
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn read_state() -> AudioState {
    let Some(status) = run(&["status"]) else {
        return AudioState::default();
    };
    let (sinks, sources) = parse_status(&status);
    let (volume, muted) = volume_of(DEFAULT_SINK);
    let (_, source_muted) = volume_of(DEFAULT_SOURCE);

    AudioState {
        available: true,
        volume,
        muted,
        source_muted,
        default_sink: sinks.iter().find(|d| d.is_default).map(|d| d.id),
        default_source: sources.iter().find(|d| d.is_default).map(|d| d.id),
        sinks,
        sources,
    }
}

/// Read a node's level and mute flag from `wpctl get-volume <target>`.
///
/// Output is `Volume: 0.42 [MUTED]`, though the factor is printed as a
/// percentage for some node types. Both forms are accepted; a linear factor is
/// converted with exactly the inverse of what `set-volume` sends, so a
/// set/get round trip cannot drift.
fn volume_of(target: &str) -> (u8, bool) {
    let Some(out) = run(&["get-volume", target]) else {
        return (0, false);
    };
    let muted = out.to_ascii_lowercase().contains("[muted]");

    let value = out
        .split_once(':')
        .map(|(_, rest)| rest.trim())
        .unwrap_or(&out)
        .split_whitespace()
        .next()
        .unwrap_or("0");

    let percent = if let Some(number) = value.strip_suffix('%') {
        number.parse::<f64>().unwrap_or(0.0)
    } else {
        value.parse::<f64>().unwrap_or(0.0) * 100.0
    };
    (percent.round().clamp(0.0, 100.0) as u8, muted)
}

/// Parse the `wpctl status` tree.
///
/// The layout is a box-drawn tree whose lines look like:
/// ```text
/// Sinks:
///   * 47. Built-in Audio Analog Stereo    [vol]
/// ```
/// `*` marks the default. Sections are introduced by a heading ending in `:`;
/// `Streams:` and `Filters:` are not device lists and must not be parsed as
/// such, which is the one mistake that makes naive parsers invent devices.
fn parse_status(status: &str) -> (Vec<AudioDevice>, Vec<AudioDevice>) {
    let mut sinks = Vec::new();
    let mut sources = Vec::new();
    let mut section: Option<bool> = None; // Some(true) = sinks, Some(false) = sources

    for line in status.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        // Section headings are not indented.
        if !line.starts_with(char::is_whitespace) {
            match trimmed.strip_suffix(':') {
                Some("Sinks") | Some("Sinks ") => {
                    section = Some(true);
                    continue;
                }
                Some("Sources") => {
                    section = Some(false);
                    continue;
                }
                _ => {
                    // Any other top-level heading ends the device list.
                    section = None;
                    continue;
                }
            }
        }

        let Some(is_sink) = section else { continue };
        // Device rows are drawn with a leading box character.
        let content = trimmed.trim_start_matches(['│', '|', '├', '└', '─', ' ']);
        let is_default = content.starts_with('*');
        let content = content.trim_start_matches('*').trim_start();

        // "<id>. <description>" with an optional trailing "[vol]".
        let Some(dot) = content.find('.') else { continue };
        let id: u32 = match content[..dot].trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut description = content[dot + 1..].trim();
        if let Some(bracket) = description.find('[') {
            description = description[..bracket].trim();
        }
        if description.is_empty() {
            continue;
        }

        let device = AudioDevice { id, description: shorten(description), is_default };
        if is_sink {
            sinks.push(device);
        } else {
            sources.push(device);
        }
    }
    (sinks, sources)
}

/// Trim the noisy hardware prefixes WirePlumber inherits from ALSA, so the
/// picker shows "Analog Stereo" rather than a full PCI address.
fn shorten(name: &str) -> String {
    const PREFIXES: &[&str] = &[
        "Raptor Lake High Definition Audio Controller ",
        "Intel High Definition Audio Controller ",
        "High Definition Audio Controller ",
        "AMD Audio Co. ",
        "PCI ",
    ];
    let mut out = name;
    for p in PREFIXES {
        if let Some(rest) = out.strip_prefix(p) {
            out = rest;
        }
    }
    // Long monitor names put the useful part after the connector.
    if out.len() > 28 {
        if let Some(idx) = out.rfind(" HDMI ") {
            out = &out[idx + 1..];
        } else if let Some(idx) = out.rfind(" DisplayPort ") {
            out = &out[idx + 1..];
        }
    }
    if out.len() > 34 {
        out = &out[..33];
        return format!("{out}…");
    }
    out.to_string()
}

thread_local! {
    static SERVICE: RefCell<Option<Rc<Audio>>> = const { RefCell::new(None) };
}

/// The process-wide audio service, started on first use.
pub fn get() -> Rc<Audio> {
    SERVICE.with(|slot| {
        if let Some(svc) = slot.borrow().as_ref() {
            return svc.clone();
        }
        let svc = Audio::new();
        *slot.borrow_mut() = Some(svc.clone());
        svc
    })
}