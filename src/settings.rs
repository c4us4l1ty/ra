//! Minimal settings editor for the config file.
//!
//! Two-way TOML: reads `config/ra.toml` (or the user-configured path), shows
//! the settings that change at runtime, and writes back with
//! `Config::save()`. Not a full panel designer — the schema is too large for
//! a single GTK window — but sufficient to change quick-settings toggles,
//! general preferences, and see current panel identity.
//!
//! Use: `ra --settings`

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use crate::config::{self, Config};

pub fn open(path: PathBuf) {
    let app = gtk4::Application::builder()
        .application_id("dev.c4us4l1ty.ra.settings")
        .build();

    let path_rc = Rc::new(path);
    let path_rc_clone = Rc::clone(&path_rc);
    app.connect_activate({
        move |app| {
            let cfg = match Config::load(&*path_rc_clone) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("cannot load config from {}: {e}", path_rc_clone.display());
                    return;
                }
            };

            let window = gtk4::ApplicationWindow::builder()
                .application(app)
                .title("ra — settings")
                .default_width(520)
                .default_height(640)
                .build();
            window.add_css_class("ra-settings");

            let scroll = gtk4::ScrolledWindow::builder()
                .vscrollbar_policy(gtk4::PolicyType::Automatic)
                .build();
            let boxv = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
            boxv.set_margin_start(18);
            boxv.set_margin_end(18);
            boxv.set_margin_top(18);
            boxv.set_margin_bottom(18);

            let header = gtk4::Label::new(Some("ra — settings"));
            header.set_halign(gtk4::Align::Start);
            header.add_css_class("title-2");
            boxv.append(&header);

            let qs = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            qs.add_css_class("ra-settings-group");
            qs.append(&gtk4::Label::builder().label("Quick Settings").css_classes(["heading"]).build());

            let enabled = gtk4::CheckButton::with_label("Enable quick-settings popup");
            enabled.set_active(cfg.quick_settings.enabled);
            qs.append(&enabled);

            let daemon = gtk4::CheckButton::with_label("Own notification daemon (opt-in)");
            daemon.set_active(cfg.quick_settings.notification_daemon);
            daemon.set_tooltip_text(Some("Off by default. Turning on asks existing daemon to stand down."));
            qs.append(&daemon);

            let notifications = gtk4::CheckButton::with_label("Show notification list");
            notifications.set_active(cfg.quick_settings.notifications);
            qs.append(&notifications);

            boxv.append(&qs);

            let gen = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            gen.add_css_class("ra-settings-group");
            gen.append(&gtk4::Label::builder().label("General").css_classes(["heading"]).build());

            let watch = gtk4::CheckButton::with_label("Watch config for changes");
            watch.set_active(cfg.general.watch_config);
            gen.append(&watch);

            let close_blur = gtk4::CheckButton::with_label("Close popup on blur");
            close_blur.set_active(cfg.general.close_popup_on_blur);
            gen.append(&close_blur);

            boxv.append(&gen);

            let panels_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            panels_box.add_css_class("ra-settings-group");
            panels_box.append(&gtk4::Label::builder().label("Panels (edit in TOML)").css_classes(["heading"]).build());
            for p in &cfg.panels {
                let text = format!("{} — {:?} / {:?} / {}px / {} / monitor={:?}",
                    p.name, p.edge, p.layer, p.height,
                    if p.exclusive { "exclusive" } else { "overlay" }, p.monitor);
                let row = gtk4::Label::builder().label(&text).xalign(0.0).build();
                row.add_css_class("ra-settings-row");
                panels_box.append(&row);
            }
            boxv.append(&panels_box);

            let save_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            save_row.set_halign(gtk4::Align::End);
            save_row.set_margin_top(12);
            let status = gtk4::Label::new(None);
            status.add_css_class("ra-status");
            save_row.append(&status);

            let save_btn = gtk4::Button::with_label("Save");
            save_btn.set_css_classes(&["suggested-action"]);
            save_btn.connect_clicked({
                let cfg = Rc::new(RefCell::new(cfg));
                let path_rc = Rc::clone(&path_rc);
                move |_| {
                    let borrowed = cfg.borrow();
                    let mut cfg = (*borrowed).clone();
                    cfg.quick_settings.enabled = enabled.is_active();
                    cfg.quick_settings.notification_daemon = daemon.is_active();
                    cfg.quick_settings.notifications = notifications.is_active();
                    cfg.general.watch_config = watch.is_active();
                    cfg.general.close_popup_on_blur = close_blur.is_active();
                    drop(borrowed);
                    match cfg.save(&*path_rc) {
                        Ok(()) => {
                            status.set_text("Saved to config/ra.toml");
                            status.remove_css_class("ra-error");
                            status.add_css_class("ra-ok");
                        }
                        Err(e) => {
                            status.set_text(&format!("Save failed: {e}"));
                            status.remove_css_class("ra-ok");
                            status.add_css_class("ra-error");
                        }
                    }
                }
            });
            save_row.append(&save_btn);

            let note = gtk4::Label::builder()
                .label("Panel layout edited in TOML. This window changes quick-settings and general prefs.")
                .wrap(true)
                .xalign(0.0)
                .build();
            note.add_css_class("ra-settings-note");
            boxv.append(&note);

            boxv.append(&save_row);

            scroll.set_child(Some(&boxv));
            window.set_child(Some(&scroll));
            window.present();
        }
    });
    app.run();
}

pub fn install() {
    use std::path::PathBuf;
    let path = crate::config::watch::resolve_path(None);
    open(path);
}
