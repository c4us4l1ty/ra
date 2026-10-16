1. The strategy: use Wayle as the shell, Quick Settings as a reference

The key is to reuse the projects' working code, not merge their entire codebases blindly.

Wayle should be your foundation: it already provides a Rust/GTK4 shell architecture with a bar, notifications, on-screen displays, device controls, configurable layouts and themes. 
GitHub
+1

Quick Settings (parth-sarthi-code/quick-settings) should be a reference for individual features and UI patterns. It has service implementations for NetworkManager, BlueZ, WirePlumber, brightness and power controls. Its top-bar integration is designed around Niri, so that part needs adapting for your target desktop. 
parth-sarthi-code/quick-settings · GitHub
+1

The goal is one cohesive project of your own, perhaps called gnomebar-rs, that runs as the top panel and Quick Settings shell on XFCE.

2. Architecture for the finished project

GNOME-like top bar

Activities · centered clock · system indicators

Quick Settings popup

Wi-Fi · Bluetooth · audio · brightness · battery · power

Theme engine

GTK theme, CSS, icons

System services

D-Bus, PipeWire, hardware

One essential distinction: the bar and popup are normal Rust/GTK application components, but pinning them to the screen edge and reserving space are Wayland compositor responsibilities. Wayle requires wlr-layer-shell; its documented compositor integrations currently target Hyprland, Niri and Mango, with Sway support in development. That does not guarantee compatibility with XFCE's Wayland compositor. 
GitHub
+1

3. Step-by-step development plan
Phase 1 — Fork Wayle and make your own project

Create a fork of 
Wayle on GitHub 
, then clone your fork:

bash
mkdir -p ~/Projects
cd ~/Projects

git clone https://github.com/YOUR-USERNAME/wayle.git gnomebar-rs
cd gnomebar-rs

Replace YOUR-USERNAME with your GitHub username. You can fork the repository using GitHub's Fork button first.

Keep the upstream repository as a remote, so you can pull in bug fixes later:

bash
git remote add upstream https://github.com/wayle-rs/wayle.git
git remote -v

Build the unmodified project before changing anything:

bash
cargo build --release

This establishes a working baseline. If the build fails, fix that first rather than mixing build issues with your changes.

Phase 2 — Study the relevant source files

Use a graphical editor such as VS Code to explore the code.

Look for these responsibilities in Wayle's source:

Bar layout and positioning

Dashboard and Quick Settings components

Network, Bluetooth, audio and brightness services

Theme loading and CSS

Configuration parsing and live reload

Startup and process management

Then inspect the 
Quick Settings repository 
 for the equivalent service implementations.

Do not copy entire files over matching filenames. Compare the APIs, data models, error handling and dependencies first. Prefer porting one useful feature at a time.

Phase 3 — Create the GNOME-like appearance

Implement the visual design before changing low-level desktop behavior.

Area

	

Target




Left side

	

Activities button or app launcher




Center

	

Clock and calendar popup




Right side

	

Network, Bluetooth, volume, battery and power




Quick Settings

	

Rounded interactive tiles and sliders




Notifications

	

Notification list and Do Not Disturb




Appearance

	

GNOME-like spacing, icons, animations and typography

Keep theme-sensitive colors separate from the layout. For example, your CSS can preserve the tile shape and spacing while using colors derived from the current GTK theme.

Phase 4 — Make the controls actually work

Reuse or implement the appropriate system interfaces:

Wi-Fi: NetworkManager D-Bus API

Bluetooth: BlueZ D-Bus API

Volume: PipeWire/WirePlumber

Brightness: brightnessctl or a supported hardware interface

Battery: UPower

Power actions: systemd-logind

Night Light: a supported color-temperature service

The controls should react to real system state changes rather than just changing their icons. Use asynchronous operations and subscribe to service events so that external changes are reflected in the UI.

Phase 5 — Solve the XFCE Wayland panel problem

This is the biggest architectural decision.

A full-screen-edge bar needs more than a GTK window. It must be placed above normal windows, remain visible across workspaces, handle multiple monitors, and reserve screen space so applications do not appear underneath it.

Your choices are:

Use Wayle's existing layer-shell implementation if the compositor you choose supports the required protocol and integrations.

Adapt the panel backend for the actual compositor used by your XFCE Wayland session.

Use an X11 panel implementation only if you decide to run XFCE under X11 instead.

Do not remove the original panel until your new bar starts reliably after login and works on every monitor you need.

Phase 6 — Package your fork

Once the shell works, provide a GitHub release with:

A Fedora RPM package

Installation and removal instructions

An autostart entry or user systemd service

Default configuration and theme files

A recovery procedure if the panel fails

Preserved license notices for reused code

That will make it much closer to a proper desktop component rather than a manually launched development project.

4. My strongest recommendation

Build on Wayle, port selected controls from Quick Settings, and keep the desktop integration behind a separate Rust module. That way, if you later change compositors, you won't have to rewrite the whole UI.

Before committing to this design, check one thing: which compositor will you use when you switch to XFCE? Fedora's XFCE Wayland session may not have the same layer-shell support as the compositors Wayle currently targets. The 
Wayle requirements 
 explain its current support.

If you want the easiest path to a working result, I'd first get the modified shell running in a compatible Wayland session, then tackle XFCE integration as a separate milestone.




IMPORTANT THINGS:
1. I dont want the weather feature in the panel or anywhere else.
2. Quick settings like Wi-fi, battery, bluetooth must not be seperate, i should be able to click them in a android like quick settings panel without opening another app.
3. This is for XFCE (wayland) - I plan to completely remove the XFCE panel and replace it with this.
4. I should be able to add multiple panels in my XFCE desktop.
5. The panel MUST be super modular (Meaning: I should be able to change it however i want to my liking, everything should super changable and customizable)
6. I will also frequently change the GTK theme in XFCE so the panel should adjust accordingly.
7. Main techstack is RUST - You can decide other languages that fit for the project as well as the GTK theme.
8. I will likely have 2 of these panels, one at the top as you see in the images in the UI plan folder and one at the bottom that will be my app dock. (SO it MUST be super customizable)
9. You can see wayle and quick-settings-manager to copy code or take from them.
