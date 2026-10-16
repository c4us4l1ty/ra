//! Configuration schema and loading.
//!
//! The whole panel is driven by a single TOML file. Design goals:
//!
//! * **Modular** - a panel is three ordered lists of slots, and a slot is a
//!   module, a module with a CSS class, or a named group of modules. Nothing
//!   about the layout is hard-coded.
//! * **Multi-panel** - any number of `[[panel]]` entries, each on its own
//!   edge/monitor, so a top bar and a bottom dock are just two entries.
//! * **Reloadable** - the file is watched and applied in place.
//!
//! Typo protection: structural keys use `deny_unknown_fields` so a misspelled
//! `[[panel]]` key is a startup error with a line number rather than a
//! setting that silently does nothing. Per-module `args` are validated by the
//! module itself, which keeps the schema open to new modules.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gtk4::prelude::*;
use serde::Deserialize;

pub mod watch;

pub const DEFAULT_CONFIG: &str = include_str!("../config/ra.toml");

/// Screen edge a panel is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
}

/// `wlr-layer-shell` layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Bottom,
    Top,
    Overlay,
}

/// Which output a panel appears on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Monitor {
    /// The first output GDK reports. GTK4 has no primary-monitor concept any
    /// more, so this is simply "the one GDK lists first", which on labwc is
    /// the compositor's first output.
    First,
    /// Every output, one window each.
    All,
    /// A connector name such as `eDP-1` or `DP-2`.
    Named(String),
}

impl Monitor {
    /// Resolve against the live monitor list. Returns every matching monitor,
    /// so `"all"` and a name that is temporarily unplugged are both handled.
    pub fn resolve(&self, monitors: &[gtk4::gdk::Monitor]) -> Vec<gtk4::gdk::Monitor> {
        match self {
            Self::All => monitors.to_vec(),
            Self::Named(name) => monitors.iter().filter(|m| monitor_name(m).eq_ignore_ascii_case(name)).cloned().collect(),
            Self::First => monitors.iter().take(1).cloned().collect(),
        }
    }
}

/// Connector name for a monitor, e.g. `eDP-1`.
///
/// GDK exposes this on Wayland. On X11 and on compositors that do not report
/// it, fall back to the model string and finally to the geometry, so that
/// `monitor = "DP-1"` still has a chance of matching and logs stay readable.
pub fn monitor_name(m: &gtk4::gdk::Monitor) -> String {
    if let Some(c) = m.connector() {
        if !c.is_empty() {
            return c.to_string();
        }
    }
    if let Some(model) = m.model() {
        if !model.is_empty() {
            return model.to_string();
        }
    }
    let g = m.geometry();
    format!("{}x{}+{}+{}", g.width(), g.height(), g.x(), g.y())
}

// ---------------------------------------------------------------------------
// Slots
// ---------------------------------------------------------------------------

/// A reference to a module, either as a bare name or with options.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ModuleRef {
    /// `"clock"`
    Name(String),
    /// `{ module = "clock", class = "big", args = { format = "%H:%M" } }`
    Detailed(Box<ModuleSpec>),
}

impl ModuleRef {
    pub fn spec(&self) -> ModuleSpec {
        match self {
            Self::Name(n) => ModuleSpec { module: n.clone(), class: None, args: None },
            Self::Detailed(d) => (**d).clone(),
        }
    }
}

/// A single module placed in a panel section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleSpec {
    /// Registered module name. `"custom:foo"` looks up a user module.
    pub module: String,
    /// Extra CSS classes, so the same module can look different in two places.
    #[serde(default)]
    pub class: Option<String>,
    /// Module-specific options, interpreted by the module itself.
    #[serde(default)]
    pub args: Option<toml::Value>,
}

impl ModuleSpec {
    /// Parse this module's `args` into its own option type. Unknown or
    /// malformed args are reported with the module name attached.
    pub fn args_as<T: serde::de::DeserializeOwned + Default>(&self) -> T {
        match &self.args {
            None => T::default(),
            Some(v) => v.clone().try_into().unwrap_or_else(|e| {
                crate::warn!("module `{}`: bad args ({e}), using defaults", self.module);
                T::default()
            }),
        }
    }
}

/// Several modules sharing one container, addressable in CSS by `#name`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// Becomes the container's CSS id.
    pub name: String,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub tooltip: Option<String>,
    pub modules: Vec<ModuleRef>,
}

/// What a slot renders.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Slot {
    /// A plain or configured module.
    Module(ModuleRef),
    /// A named group.
    Group(Group),
}

// ---------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------

/// Per-panel edge insets, in pixels. Each edge is optional; unset means 0.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Margin {
    #[serde(default)]
    pub top: i32,
    #[serde(default)]
    pub bottom: i32,
    #[serde(default)]
    pub left: i32,
    #[serde(default)]
    pub right: i32,
}

/// One panel: one window (or one window per monitor) on one edge.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    /// Identifier, used in logs and as the default CSS class (`panel-<name>`).
    #[serde(default = "default_name")]
    pub name: String,
    /// `primary` (default), `all`, or a connector name.
    #[serde(default = "default_monitor")]
    pub monitor: MonitorSpec,
    #[serde(default)]
    pub edge: Edge,
    #[serde(default = "default_layer")]
    pub layer: Layer,
    /// Panel thickness in pixels. Applies to the anchored axis only.
    #[serde(default = "default_height")]
    pub height: u32,
    /// Reserve screen space so windows are not covered. This is what makes the
    /// panel behave like a real panel rather than an overlay.
    #[serde(default = "yes")]
    pub exclusive: bool,
    #[serde(default)]
    pub margin: Margin,
    /// Wayland layer-shell namespace. `None` derives it from `name`.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Extra CSS classes applied to the panel window.
    #[serde(default)]
    pub class: Option<String>,
    /// Gap between widgets, in pixels.
    #[serde(default)]
    pub spacing: Option<u32>,

    #[serde(default)]
    pub left: Vec<Slot>,
    #[serde(default)]
    pub center: Vec<Slot>,
    #[serde(default)]
    pub right: Vec<Slot>,
}

fn default_name() -> String {
    "panel".to_string()
}
fn default_monitor() -> MonitorSpec {
    MonitorSpec::First
}
fn default_layer() -> Layer {
    Layer::Top
}
fn default_height() -> u32 {
    32
}
fn yes() -> bool {
    true
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            name: default_name(),
            monitor: default_monitor(),
            edge: Edge::Top,
            layer: default_layer(),
            height: default_height(),
            exclusive: true,
            margin: Margin::default(),
            namespace: None,
            class: None,
            spacing: None,
            left: Vec::new(),
            center: Vec::new(),
            right: Vec::new(),
        }
    }
}

impl Panel {
    pub fn monitor(&self) -> Monitor {
        match &self.monitor {
            MonitorSpec::First => Monitor::First,
            MonitorSpec::All => Monitor::All,
            MonitorSpec::Named(n) => Monitor::Named(n.clone()),
        }
    }

    pub fn css_classes(&self) -> Vec<String> {
        let mut v = vec![format!("panel-{}", self.name), format!("panel-edge-{}", edge_str(self.edge))];
        if let Some(c) = &self.class {
            v.extend(c.split_whitespace().map(str::to_string));
        }
        v
    }
}

pub fn edge_str(e: Edge) -> &'static str {
    match e {
        Edge::Top => "top",
        Edge::Bottom => "bottom",
        Edge::Left => "left",
        Edge::Right => "right",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorSpec {
    First,
    All,
    Named(String),
}

impl<'de> Deserialize<'de> for MonitorSpec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Named(String),
            NamedTable { monitor: String },
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Named(s) | Raw::NamedTable { monitor: s } => {
                let trimmed = s.trim();
                // Only the aliases are case-insensitive. A connector name is
                // *not*: `eDP-1` and `DP-2` are different physical ports, and
                // lowercasing them would make the log misreport what is on
                // screen. Matching in `Monitor::resolve` is already
                // case-insensitive, so this is purely about reporting fidelity.
                match trimmed.to_ascii_lowercase().as_str() {
                    "primary" | "first" | "*" | "any" => MonitorSpec::First,
                    "all" => MonitorSpec::All,
                    _ => MonitorSpec::Named(trimmed.to_string()),
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Quick settings
// ---------------------------------------------------------------------------

/// Where the quick-settings window is placed relative to its anchor panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PopupAnchor {
    TopRight,
    TopLeft,
    TopCenter,
    BottomRight,
    BottomLeft,
    BottomCenter,
}

/// Order and visibility of the quick-settings tiles. Anything omitted is not
/// built at all, so a panel that does not want brightness does not pay for it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuickSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_popup_width")]
    pub width: u32,
    #[serde(default = "default_anchor")]
    pub anchor: PopupAnchor,
    /// Gap between the panel and the popup, in pixels.
    #[serde(default = "default_gap")]
    pub gap: u32,
    /// Tiles, in display order.
    #[serde(default = "default_tiles")]
    pub tiles: Vec<String>,
    /// Show the volume row (slider + output picker).
    #[serde(default = "default_true")]
    pub volume: bool,
    /// Show the brightness row.
    #[serde(default = "default_true")]
    pub brightness: bool,
    /// Show the microhone mute toggle.
    #[serde(default = "default_true")]
    pub microphone: bool,
    /// Poll interval for values with no change signal (brightness), in ms.
    #[serde(default = "default_poll_ms")]
    pub poll_interval_ms: u32,
    /// Show the notification list below the tiles.
    #[serde(default = "default_false")]
    pub notifications: bool, // Read by build_pages; used at build time
    /// Become the notification server, claiming `org.freedesktop.Notifications`.
    ///
    /// Off by default on purpose. Turning it on routes every notification on
    /// the desktop through ra and asks whatever daemon is already running to
    /// stand down; if ra then stops, notifications go nowhere until it returns.
    /// See `src/service/notification.rs` for the full trade-off.
    #[serde(default)]
    pub notification_daemon: bool,
}

fn default_true() -> bool {
    true
}
fn default_false() -> bool {
    false
}
fn default_popup_width() -> u32 {
    380
}
fn default_anchor() -> PopupAnchor {
    PopupAnchor::TopRight
}
fn default_gap() -> u32 {
    8
}
fn default_poll_ms() -> u32 {
    1000
}

/// Tiles used when a configuration sets `[quick_settings]` but no `tiles` key.
///
/// Must match the `tiles` list in the shipped `config/ra.toml`, and every name
/// must be implemented: an unknown name is skipped with a warning, so a typo
/// here would cost a tile silently. `every_default_tile_is_implemented` is the
/// test that catches it.
fn default_tiles() -> Vec<String> {
    [
        "network",
        "bluetooth",
        "power-profile",
        "night-light",
        "dark-style",
        "do-not-disturb",
        "airplane",
        "microphone",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for QuickSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            width: default_popup_width(),
            anchor: default_anchor(),
            gap: default_gap(),
            tiles: default_tiles(),
            notifications: true,
            notification_daemon: false,
            volume: true,
            brightness: true,
            microphone: true,
            poll_interval_ms: default_poll_ms(),
        }
    }
}

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Style {
    /// Extra CSS appended after the built-in stylesheet. Missing file is a
    /// warning, not a fatal error.
    #[serde(default)]
    pub stylesheet: Option<PathBuf>,
    /// Force the accent colour instead of deriving it from the GTK theme.
    #[serde(default)]
    pub accent: Option<String>,
    /// Re-derive colours when the GTK theme changes. The user switches themes
    /// often, so this defaults to on.
    #[serde(default = "default_true")]
    pub follow_gtk_theme: bool,
    /// Opacity of panel backgrounds, 0.0 - 1.0. `None` keeps the theme's own.
    #[serde(default)]
    pub opacity: Option<f64>,
}

impl Default for Style {
    fn default() -> Self {
        Self { stylesheet: None, accent: None, follow_gtk_theme: true, opacity: None }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    /// Reload the config when the file changes.
    #[serde(default = "default_true")]
    pub watch_config: bool,
    /// Close the quick settings when it loses focus.
    #[serde(default = "default_true")]
    pub close_popup_on_blur: bool,
    /// Debounce for config reloads, in ms. Editors write files in bursts.
    #[serde(default = "default_debounce")]
    pub reload_debounce_ms: u32,
    /// Emit a warning when a configured module does not exist.
    #[serde(default = "default_true")]
    pub strict_modules: bool,
}

fn default_debounce() -> u32 {
    200
}

impl Default for General {
    fn default() -> Self {
        Self { watch_config: true, close_popup_on_blur: true, reload_debounce_ms: default_debounce(), strict_modules: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub style: Style,
    #[serde(default)]
    pub quick_settings: QuickSettings,
    #[serde(default, rename = "panel")]
    pub panels: Vec<Panel>,
    /// Named reusable sections, referenced as `"@name"` from a panel.
    #[serde(default)]
    pub include: BTreeMap<String, Vec<Slot>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General::default(),
            style: Style::default(),
            quick_settings: QuickSettings::default(),
            panels: vec![Panel::default()],
            include: BTreeMap::new(),
        }
    }
}

/// Push the module names of a group's member list into `out`.
fn collect_modules(modules: &[ModuleRef], out: &mut Vec<String>) {
    for m in modules {
        out.push(m.spec().module);
    }
}

impl Config {
    /// Parse TOML text.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Load a config file, falling back to the built-in defaults when the file
    /// is missing. A malformed file is an error.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
                std::fs::write(path, DEFAULT_CONFIG)
                    .map_err(|e| format!("cannot write default config to {}: {e}", path.display()))?;
                crate::info!("wrote default config to {}", path.display());
                Ok(Self::parse(DEFAULT_CONFIG).expect("built-in config must parse"))
            }
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// Expand `"@name"` references in a panel section against `include`.
    /// Recursion is bounded so a self-referential include cannot hang startup.
    pub fn expand_slots(slots: &[Slot], includes: &BTreeMap<String, Vec<Slot>>) -> Vec<Slot> {
        const MAX_DEPTH: usize = 8;
        fn walk(slots: &[Slot], inc: &BTreeMap<String, Vec<Slot>>, depth: usize, out: &mut Vec<Slot>) {
            for slot in slots {
                match slot {
                    Slot::Module(ModuleRef::Name(n)) if n.starts_with('@') && depth < MAX_DEPTH => {
                        if let Some(inner) = inc.get(&n[1..]) {
                            walk(inner, inc, depth + 1, out);
                        } else {
                            out.push(slot.clone());
                        }
                    }
                    other => out.push(other.clone()),
                }
            }
        }
        let mut out = Vec::new();
        walk(slots, includes, 0, &mut out);
        out
    }

    /// Every distinct module name referenced by the config, for `--diagnose`.
    pub fn module_names(&self) -> Vec<String> {
        fn collect(slots: &[Slot], out: &mut Vec<String>) {
            for slot in slots {
                match slot {
                    Slot::Module(m) => out.push(m.spec().module),
                    Slot::Group(g) => collect_modules(&g.modules, out),
                }
            }
        }
        let mut names = Vec::new();
        for panel in &self.panels {
            for section in [&panel.left, &panel.center, &panel.right] {
                collect(section, &mut names);
            }
        }
        names.sort();
        names.dedup();
        names
    }
}

/// `~/.config/ra/config.toml`
pub fn user_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
            home.join(".config")
        });
    base.join("ra").join("config.toml")
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The configuration the installer ships must always parse. If it does not,
    /// every first run starts on the built-in fallback instead of the documented
    /// defaults, which is exactly the kind of bug nobody notices.
    #[test]
    fn shipped_default_config_is_valid() {
        let config = Config::parse(DEFAULT_CONFIG).expect("config/ra.toml must parse");
        assert_eq!(config.panels.len(), 2, "the default ships a bar and a dock");
        assert!(!config.panels[0].left.is_empty());
        assert!(!config.panels[0].center.is_empty());
        assert!(!config.panels[0].right.is_empty());
        assert_eq!(config.panels[0].edge, Edge::Top);
        assert_eq!(config.panels[1].edge, Edge::Bottom);
        assert!(config.quick_settings.enabled);
    }

    #[test]
    fn every_module_in_the_default_config_exists() {
        let config = Config::parse(DEFAULT_CONFIG).unwrap();
        for name in config.module_names() {
            assert!(
                crate::module::known().contains(&name.as_str()),
                "the default config uses `{name}`, which this build does not provide"
            );
        }
    }

    #[test]
    fn a_slot_can_be_a_name_a_table_or_a_group() {
        let config = Config::parse(
            r#"
            [[panel]]
            left = [
              "launcher",
              { module = "clock", class = "big", args = { format = "%H" } },
              { name = "status", class = "s", modules = ["network", "volume"] },
            ]
            "#,
        )
        .unwrap();

        let slots = &config.panels[0].left;
        assert!(matches!(&slots[0], Slot::Module(ModuleRef::Name(n)) if n == "launcher"));

        match &slots[1] {
            Slot::Module(ModuleRef::Detailed(spec)) => {
                assert_eq!(spec.module, "clock");
                assert_eq!(spec.class.as_deref(), Some("big"));
            }
            other => panic!("expected a detailed module, got {other:?}"),
        }

        match &slots[2] {
            Slot::Group(group) => {
                assert_eq!(group.name, "status");
                assert_eq!(group.modules.len(), 2);
            }
            other => panic!("expected a group, got {other:?}"),
        }

        assert_eq!(config.module_names(), vec!["clock", "launcher", "network", "volume"]);
    }

    #[test]
    fn monitor_accepts_both_spellings() {
        let from_string = Config::parse("[[panel]]\nmonitor = \"DP-2\"\n").unwrap();
        assert_eq!(from_string.panels[0].monitor, MonitorSpec::Named("DP-2".into()));

        // `primary` is an alias for the first output: GTK4 dropped the
        // primary-monitor concept, so this is what people actually type.
        for alias in ["primary", "first", "*", "any"] {
            let config = Config::parse(&format!("[[panel]]\nmonitor = \"{alias}\"\n")).unwrap();
            assert_eq!(config.panels[0].monitor, MonitorSpec::First, "alias {alias}");
        }
        assert_eq!(
            Config::parse("[[panel]]\nmonitor = \"all\"\n").unwrap().panels[0].monitor,
            MonitorSpec::All
        );
    }

    /// A misspelled structural key must be a startup error, not a setting that
    /// silently does nothing.
    #[test]
    fn unknown_structural_keys_are_rejected() {
        assert!(Config::parse("[[panel]]\nedg = \"top\"\n").is_err());
        assert!(Config::parse("[[panel]]\nleft = [{ modul = \"clock\" }]\n").is_err());
        assert!(Config::parse("[style]\nfolow_gtk_theme = true\n").is_err());
    }

    /// Module arguments are the module's own business, so unknown keys there
    /// are its problem, not the schema's.
    #[test]
    fn unknown_module_args_are_left_to_the_module() {
        let config = Config::parse(
            r#"
            [[panel]]
            left = [{ module = "clock", args = { formt = "%H" } }]
            "#,
        )
        .unwrap();
        assert_eq!(config.panels[0].left.len(), 1);
    }

    #[test]
    fn includes_are_expanded() {
        let config = Config::parse(
            r#"
            [include]
            status = ["network", "volume"]

            [[panel]]
            left = ["launcher", "@status"]
            "#,
        )
        .unwrap();

        let expanded = Config::expand_slots(&config.panels[0].left, &config.include);
        let names: Vec<String> = expanded
            .iter()
            .map(|s| match s {
                Slot::Module(m) => m.spec().module,
                Slot::Group(_) => "group".into(),
            })
            .collect();
        assert_eq!(names, vec!["launcher", "network", "volume"]);
    }

    #[test]
    fn a_missing_include_is_left_alone_rather_than_dropped() {
        let config = Config::parse("[[panel]]\nleft = [\"@nope\"]\n").unwrap();
        let expanded = Config::expand_slots(&config.panels[0].left, &config.include);
        assert_eq!(expanded.len(), 1, "an unresolved include stays visible as a config error");
    }

    /// A self-referential include must terminate rather than hang startup.
    #[test]
    fn include_recursion_is_bounded() {
        let mut includes = BTreeMap::new();
        includes.insert("loop".to_string(), vec![Slot::Module(ModuleRef::Name("@loop".into()))]);
        let slots = vec![Slot::Module(ModuleRef::Name("@loop".into()))];
        let expanded = Config::expand_slots(&slots, &includes);
        assert!(expanded.len() <= 9, "expansion terminated with {} entries", expanded.len());
    }

    #[test]
    fn module_args_default_when_absent_or_wrong() {
        let spec = ModuleSpec { module: "clock".into(), class: None, args: None };
        assert_eq!(spec.args_as::<Shape>(), Shape::default());

        let bad = ModuleSpec {
            module: "clock".into(),
            class: None,
            args: Some(toml::toml! { format = 42 }.into()),
        };
        assert_eq!(bad.args_as::<Shape>(), Shape::default(), "bad args fall back, they do not panic");
    }

    /// A tile name that no build implements is skipped with a warning, so a
    /// typo in the shipped defaults would cost a tile *silently* - no error, no
    /// crash, just one fewer square in the popup. This is the only place that
    /// can catch it.
    #[test]
    fn every_default_tile_is_implemented() {
        let known = crate::popup::known_tiles();
        for tile in default_tiles() {
            assert!(
                known.contains(&tile.as_str()),
                "the default tile `{tile}` is not implemented; known tiles are {known:?}"
            );
        }
    }

    /// The shipped config and the fallback defaults must offer the same tiles,
    /// or `tiles = [...]` would visibly change the popup when it is omitted.
    #[test]
    fn the_shipped_config_matches_the_fallback_defaults() {
        let shipped = Config::parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(
            shipped.quick_settings.tiles,
            default_tiles(),
            "config/ra.toml lists different tiles than the built-in fallback"
        );
    }

    #[derive(Debug, Default, PartialEq, Deserialize)]
    struct Shape {
        #[serde(default)]
        format: Option<String>,
    }
}
