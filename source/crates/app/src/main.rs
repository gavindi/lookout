//! GTK4/libadwaita application entry point.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
mod app_config;
mod background;
mod background_image;
mod calendar_colors;
mod calendar_view;
mod compose;
mod config_view;
mod contacts_editor;
mod contacts_view;
mod event_editor;
mod folder_tree;
mod goa_calendar_credentials;
mod goa_credentials;
mod google_tasks;
mod identities;
mod last_view;
mod lookout_view;
mod mail_notifications;
mod message_header;
mod message_list;
mod microsoft_oauth;
mod online_accounts;
mod other_accounts;
mod recipient_entry;
mod recurrence;
mod reminders;
mod resources;
mod settings;
mod shortcuts;
mod tags;
mod task_editor;
mod tasks_view;
mod theme;
mod trusted_senders;
mod ui_state_db;
mod window;
mod worker;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

const APP_ID: &str = "io.github.gavindi.Lookout";

fn main() -> glib::ExitCode {
    // GTK3-era accessibility modules (`gail`, `atk-bridge`) sometimes linger
    // in the environment's `GTK_MODULES`; GTK4 ships AT-SPI support natively,
    // so it ignores them but warns "Not loading module ... Please try to not
    // load it." every time accessibility comes up (first render, a click on
    // the reading pane's WebView). Strip them before GTK reads the variable.
    if let Ok(value) = std::env::var("GTK_MODULES") {
        let kept: Vec<&str> = value
            .split(':')
            .filter(|m| !matches!(*m, "gail" | "atk-bridge"))
            .collect();
        std::env::set_var("GTK_MODULES", kept.join(":"));
    }

    tracing_subscriber::fmt::init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    let worker = Rc::new(worker::Worker::new());

    // The autostart entry launches `lookout --hidden`: build the window but
    // don't present it, so mail/calendar sync and the reminder loop run from
    // login without a window. Clicking the app icon or a notification's
    // action (`app.open-mailbox`, `app.open-event`, `app.raise-window`)
    // raises it.
    let hidden = Rc::new(Cell::new(false));
    {
        let hidden = hidden.clone();
        app.add_main_option("hidden", b'h'.into(), glib::OptionFlags::NONE, glib::OptionArg::None, "Start without showing the main window", None);
        app.connect_handle_local_options(move |_app, options| {
            // A `G_OPTION_ARG_NONE` option shows up in the parsed options
            // dictionary as a true boolean; the env-args check covers any
            // dispatch quirk, since the process argv is never rewritten.
            if options.lookup_value("hidden", None).is_some() || std::env::args().any(|arg| arg == "--hidden") {
                hidden.set(true);
            }
            std::ops::ControlFlow::<glib::ExitCode>::Continue(())
        });
    }

    // Single window for the app's lifetime: an activation while one exists
    // (an app-icon click, a second process's D-Bus activation) just presents
    // it instead of building a duplicate.
    let window: Rc<RefCell<Option<adw::ApplicationWindow>>> = Rc::new(RefCell::new(None));
    app.connect_activate(move |app| {
        let display = gtk::gdk::Display::default().expect("GTK initializes a display");
        crate::resources::register(&display);

        let existing = window.borrow().clone();
        if let Some(win) = existing {
            win.present();
            return;
        }
        let win = window::build_window(app, worker.clone());
        *window.borrow_mut() = Some(win.clone());
        if hidden.get() {
            tracing::debug!("started with --hidden; window built but not presented");
        } else {
            win.present();
        }
    });

    app.run()
}
