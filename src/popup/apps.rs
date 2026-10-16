//! The application launcher.
//!
//! Rather than parsing `.desktop` files itself, this embeds GTK's
//! `AppChooserWidget`. GAppInfo already implements the freedesktop spec
//! correctly - `NoDisplay`, `Hidden`, `OnlyShowIn`, `NotShowIn`, localisation,
//! `TryExec` - and reimplementing any of that is a reliable way to show a
//! broken launcher. Using it also means the launcher automatically knows about
//! Flatpak and Snap entries, which install their own desktop files.
//!
//! The widget lives inside the quick-settings surface rather than in its own
//! window: a new toplevel cannot take keyboard focus while the layer-shell
//! popup holds the grab, so a separate launcher window would show but not
//! accept typing.

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::ui;

/// A page holding a searchable grid of installed applications.
pub struct AppLauncher {
    pub root: gtk4::Box,
    header: ui::PageHeader,
    // `GtkAppChooserWidget` has been deprecated since GTK 4.10 but still has no
    // replacement: it is the only application chooser GTK4 offers, and it is the
    // only way to get GAppInfo's correct handling of `NoDisplay`, `Hidden`,
    // `OnlyShowIn`, localisation and Flatpak/Snap entries for free.
    #[allow(deprecated)]
    chooser: gtk4::AppChooserWidget,
}

impl AppLauncher {
    #[allow(deprecated)]
    pub fn new() -> Self {
        let root = ui::vbox(8);
        root.set_size_request(-1, 340);

        let header = ui::PageHeader::new();
        header.set_title("Applications");
        root.append(&header.root);

        // The empty content type matches every application.
        let chooser = gtk4::AppChooserWidget::new("");
        // `show_all` is off by default, which limits the list to what GAppInfo
        // considers relevant to the running desktop. XFCE sessions often have
        // no desktop id at all, which would otherwise hide everything.
        chooser.set_show_all(true);
        chooser.set_show_other(false);
        chooser.set_show_fallback(false);
        chooser.set_show_recommended(true);
        chooser.set_hexpand(true);
        chooser.set_vexpand(true);

        let status = gtk4::Label::new(None);
        status.add_css_class("ra-empty");

        root.append(&status);
        root.append(&chooser);

        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
        scroller.set_max_content_height(300);
        scroller.add_css_class("ra-scroller");
        scroller.set_child(Some(&chooser));
        root.append(&scroller);

        let status_for_activate = status.clone();
        chooser.connect_application_activated(move |widget, app| {
            if app.launch(&[], None::<&gio::AppLaunchContext>).is_err() {
                status_for_activate.set_text("That application could not be launched.");
                status_for_activate.set_visible(true);
            } else {
                status_for_activate.set_visible(false);
            }
            widget.set_default_text("");
        });

        Self { root, header, chooser }
    }

    pub fn header(&self) -> &ui::PageHeader {
        &self.header
    }

    /// Focus the search entry when the page is opened, so the user can type
    /// straight away. Requires the popup to hold the keyboard grab.
    #[allow(deprecated)]
    pub fn focus_search(&self) {
        self.chooser.grab_focus();
        let weak = self.chooser.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(chooser) = weak.upgrade() {
                chooser.grab_focus();
            }
        });
    }
}