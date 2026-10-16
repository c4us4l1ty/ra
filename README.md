# ra

A modular panel for XFCE/Wayland, written in Rust against GTK4 and
`wlr-layer-shell`.

It replaces the XFCE panel entirely: a top bar, an optional bottom dock, and
**one** Android-style quick-settings popup that holds every control — Wi-Fi,
Bluetooth, volume, brightness, power mode, night light, dark style, do not
disturb, airplane mode, the application grid and the session actions. No
separate helper applications, no panel extensions.

```
git clone https://github.com/c4us4l1ty/ra
cd ra
./install.sh
```

Then start it:

```sh
ra --diagnose    # what it can see, and which services answered
ra               # run it
```

---

## Why it looks like this

Three deliberate choices shape everything below.

**No async runtime, no D-Bus crate.** The panel runs on the GLib main loop,
which `gtk4` already pulls in. `zbus` would have added a dozen crates and an
executor for a program that is, at rest, a `poll()` on one idle loop. Everything
here uses `gio`'s GDBus bindings directly, so the dependency list stays at
`gtk4`, `gtk4-layer-shell`, `serde` and `toml`.

**Nothing polls.** NetworkManager, BlueZ, UPower, power-profiles-daemon and
logind all push state changes over D-Bus, so every service is a set of signal
subscriptions that update state on change. The two exceptions are documented
where they occur, because both are forced by the platform:

- *brightness* — the kernel offers no notification for a backlight change, so
  it reads one small sysfs file on a timer (1 s while the popup is open, 2 s
  otherwise) and emits nothing unless the value actually moved;
- *audio* — `wpctl` reads are `fork`+`exec`, so PipeWire's own `Info` signal is
  used for change detection when present, with a polled fallback.

**One surface, never two.** The quick settings is a single layer-shell window.
A second toplevel would be visible but the compositor would not give it keyboard
focus while the popup holds the grab, so the Wi-Fi passphrase prompt, the
shutdown confirmation and the application search are all built *inside* the
popup surface. That constraint is why there is no separate settings dialog.

---

## Install

`install.sh` checks dependencies, builds, installs and registers autostart:

```sh
./install.sh              # build and install
./install.sh --help       # (no options; see below)
```

Build dependencies:

| Distribution | Packages |
|---|---|
| Fedora / RHEL | `gtk4-devel gtk4-layer-shell-devel rust cargo gcc` |
| Debian / Ubuntu | `libgtk-4-dev libgtk4-layer-shell-dev cargo gcc` |
| Arch | `gtk4 gtk4-layer-shell rust gcc` |

Runtime: NetworkManager, WirePlumber (`wpctl`), bluetoothd, and optionally a
night-light back-end (`gamut`, `gammastep`, `wl-gammastep`, `redshift` or
`sct`). Missing services disable their tiles rather than breaking the panel, and
`ra --diagnose` lists what is missing.

Re-running is safe: it rebuilds, reinstalls the binary, and leaves your config
and stylesheet alone.

### Removing the XFCE panels

`install.sh` deliberately does **not** do this. Taking a desktop's panel away
before its replacement is running is how you end up with an empty desktop and
no way back. `ra` is designed to sit alongside `xfce4-panel` until you are happy
with it.

When you are:

```sh
xfconf-query -c xfce4-panel -p /panels -a        # list them
xfconf-query -c xfce4-panel -p /panels -t int -n <n> -r
```

then log out and back in. Keep one panel until you have confirmed `ra` starts
on login.

### Brightness needs a group membership

Writing to `/sys/class/backlight/*/brightness` requires membership of the
`video` or `backlight` group (which one is distribution-specific). Without it
the slider moves and then snaps back:

```sh
sudo usermod -aG video "$USER"     # or: backlight
```

Log out and back in for the group change to take effect.

---

## Configuration

`~/.config/ra/config.toml`, watched live: save it and the panel rebuilds. A
syntax error is reported in the log and the previous configuration stays in
place, so a half-typed line can never blank your bar.

A panel is three ordered lists — `left`, `center`, `right`. A slot is a module
name, a module with options, or a named group:

```toml
[[panel]]
name = "top"
monitor = "first"        # "first", "all", or a connector like "eDP-1"
edge = "top"             # top | bottom | left | right
height = 34
exclusive = true         # reserve screen space so windows are not covered
spacing = 2

left   = [ "launcher" ]
center = [ { module = "clock", args = { format = "%H:%M" } } ]
right  = [
  { name = "status", class = "ra-status",
    modules = ["network", "bluetooth", "volume", "battery"] },
]
```

Any number of `[[panel]]` blocks is allowed, which is how a top bar plus a
bottom dock is expressed.

### Modules

| Module | Arguments |
|---|---|
| `clock` | `format`, `seconds`, `tooltip`, `on_click`, `on_middle_click` |
| `label` | `text` |
| `separator` | `size` |
| `spacer` | `width`, `expand` |
| `launcher` | `icon`, `label` |
| `network` `bluetooth` `volume` `brightness` `battery` `power-profile` | `show_label`, `size` |
| `quick-settings` | `indicators`, `show_label`, `size` |
| `custom` | `command`, `args`, `interval`, `tooltip` |

`custom` runs a program and shows its first line of output. It is the one module
that polls, because asking to be told the value of something is exactly what it
is for. The command is executed directly, never through a shell.

Reusable sections live under `[include]` and are referenced as `"@name"`.

### Quick settings

```toml
[quick_settings]
width = 380
anchor = "top-right"     # or top-left, top-center, bottom-*
tiles = ["network", "bluetooth", "power-profile", "night-light",
         "dark-style", "do-not-disturb", "airplane", "microphone"]
volume = true
brightness = true
microphone = true
poll_interval_ms = 1000
```

Anything not listed is not built at all, so a panel that does not want
brightness does not pay for it.

The nine available tiles:

| Tile | Does |
|---|---|
| `network` | Wi-Fi state; opens the network list |
| `bluetooth` | Adapter power; opens the device list |
| `power-profile` | Cycles power-saver / balanced / performance |
| `night-light` | Night light, with a temperature slider |
| `dark-style` | Follows the GTK theme when on, forces dark when off |
| `do-not-disturb` | Silences notifications ra itself raises |
| `airplane` | Turns off Wi-Fi and Bluetooth together |
| `microphone` | Input mute; also switches the input device |
| `session` | Lock, suspend, restart, shut down, log out |

A name that no build implements is skipped with a warning in the log rather
than drawing an empty square, so a typo costs one tile and says so.

### Theming

Every colour in the built-in stylesheet is a **GTK named colour**
(`@window_bg_color`, `@accent_bg_color`, `@borders`…). GTK resolves those
against the active theme and re-resolves when it changes, so switching XFCE's
GTK theme re-colours the bar, the tiles and the popup with no work at all.

```toml
[style]
follow_gtk_theme = true
accent = "#3584e4"                    # optional override
opacity = 0.94                        # panel background
stylesheet = "~/.config/ra/style.css" # your CSS, appended last
```

`ra --diagnose` lists the modules this build understands and flags any module
your config references that does not exist.

---

## Keyboard shortcut

A panel never holds the keyboard grab, so a key press cannot become a click
locally — the key is captured by XFCE and delivered as a command. In XFCE's
*Settings → Keyboard → Application Shortcuts*, bind any key to:

```sh
ra --toggle     # or --open / --close
```

`--toggle` finds the running instance over the session bus and toggles its
popup, whichever panel it is anchored to.

---

## Environment

| Variable | Meaning |
|---|---|
| `RA_LOG` | `error` `warn` (default) `info` `debug` `trace` |
| `RA_AUDIO_POLL_MS` | Fallback audio poll interval; `0` disables it |

## Commands

```
ra                     run the panel
ra --diagnose          report what ra can see, then exit
ra --toggle            toggle the quick settings of a running instance
ra --open / --close    open or close it explicitly
ra -c, --config PATH   use a specific config file
ra -w, --no-watch      do not reload on config change
```

## Uninstall

```sh
./uninstall.sh          # removes the binary and autostart entries
./uninstall.sh --purge  # also removes ~/.config/ra and the saved state
```

The XFCE panels are never touched.

---

## Troubleshooting

**Text fields ignore the keyboard** (the Wi-Fi passphrase, the launcher
search). GTK4 talks to the compositor's text-input protocol directly, so this
normally just works. If your compositor does not negotiate that protocol, GTK
falls back to the old XIM path and can end up with no input method at all:

```sh
GTK_IM_MODULE=simple ra
```

If that fixes it, add `env GTK_IM_MODULE=simple` to the `Exec=` line in
`~/.config/autostart/ra.desktop`.

**The brightness slider moves and snaps back.** You are not in the group that
owns the backlight sysfs node — see
[Brightness needs a group membership](#brightness-needs-a-group-membership)
above.

**Nothing is on screen.** `ra --diagnose` reports whether `wlr-layer-shell`
is available. On XFCE/Wayland it always is; if it is not, the session is not
running labwc.

**A config change did nothing.** Set `RA_LOG=info`. A syntax error is reported
with a line number, and the previous configuration stays in place.

---

## Known limitations

Stated plainly, because a panel that hides its edges is worse than one that
names them.

- **Do Not Disturb is currently inert.** The toggle is real and it persists,
  but ra does not own a notification daemon, so there are no notifications for
  it to silence. Suppressing *other* applications' pop-ups would mean taking
  over the notification daemon, which is not something a panel should do behind
  your back. The daemon is on the roadmap and the toggle is already wired to it.
- **Audio has only been exercised by unit tests.** Volume and device switching
  go through `wpctl`, the official WirePlumber CLI, specifically so they behave
  the same across PipeWire versions. The parser is tested against captured
  output, but it has not been run against live hardware by the author.
- **A WPA network with no saved profile gets an inline passphrase prompt**
  rather than opening NetworkManager's dialog, because a second toplevel cannot
  take keyboard focus while the popup holds the grab. Enterprise (802.1x)
  networks are not supported.
- **`custom` modules run a command on a timer.** That is what they are for, but
  a slow command costs you CPU at whatever interval you chose.
- **One instance only.** A second `ra` refuses to start rather than fighting
  the first one for screen edges.

---

## Licence

MIT.