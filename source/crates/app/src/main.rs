//! GTK4/libadwaita application entry point.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
mod app_config;
mod assistant;
mod assistant_tools;
mod automatic_replies_view;
mod background;
mod background_image;
mod blocked_senders;
mod calendar_colors;
mod calendar_groups;
mod calendar_invites;
mod calendar_share;
mod calendar_tz;
mod calendar_view;
mod categories;
mod chat_links;
mod compose;
mod config_view;
mod contact_merge_view;
mod contact_picker;
mod contacts_details;
mod contacts_editor;
mod contacts_view;
mod context_menu;
mod conversation_view;
mod datetime_picker;
mod desktop_contacts;
mod directory;
mod eds_directory;
mod eds_mirror;
mod event_editor;
mod event_menu;
mod evolution_import;
mod folder_menu;
mod folder_tree;
mod follow_up;
mod goa_calendar_credentials;
mod goa_credentials;
mod google_tasks;
mod graph_account;
mod graph_automatic_replies;
mod graph_calendar_share;
mod graph_categories;
mod graph_directory;
mod graph_mail;
mod graph_pin;
mod graph_rooms;
mod graph_rules;
#[cfg(test)]
mod gtk_test;
mod http;
mod icon_style;
mod identities;
mod ignored_conversations;
mod invite_conflicts;
mod last_view;
mod launcher_entry;
mod ldap_directory;
mod ldap_directory_view;
mod link_safety;
mod lookout_view;
mod mail_notifications;
mod message_columns;
mod message_header;
mod message_list;
mod message_menu;
mod message_window;
mod microsoft_oauth;
mod my_day;
mod oauth_loopback;
mod online_accounts;
mod other_accounts;
mod privacy;
mod profile_card;
mod quick_steps;
mod quick_steps_view;
mod recipient_entry;
mod recurrence;
mod reminders;
mod reply_with_meeting;
mod resources;
mod ribbon;
mod room_finder;
mod rules;
mod rules_view;
mod schedule_grid;
mod search_folders;
mod settings;
mod shared_mailbox;
mod shortcuts;
mod signatures;
mod snooze;
mod style;
mod task_editor;
mod task_lists;
mod task_recurrence;
mod tasks_view;
mod templates;
mod theme;
mod tray;
mod trusted_senders;
mod ui_state_db;
mod util;
mod window;
mod worker;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

const APP_ID: &str = "io.github.gavindi.Lookout";

/// Command-line options matching the desktop file's `[Desktop Action]`s.
/// Each name is both the `--option` and the app action `build_window`
/// registers for it.
const LAUNCHER_ACTIONS: [(&str, &str); 4] = [
    ("compose", "Compose a new message"),
    ("contacts", "Open Contacts"),
    ("calendar", "Open Calendar"),
    ("tasks", "Open Tasks"),
];

fn main() -> glib::ExitCode {
    // GTK3-era accessibility modules (`gail`, `atk-bridge`) sometimes linger
    // in the environment's `GTK_MODULES`; GTK4 ships AT-SPI support natively,
    // so it ignores them but warns "Not loading module ... Please try to not
    // load it." every time accessibility comes up (first render, a click on
    // the reading pane's WebView). Strip them before GTK reads the variable.
    if let Ok(value) = std::env::var("GTK_MODULES") {
        let kept: Vec<&str> = value.split(':').filter(|m| !matches!(*m, "gail" | "atk-bridge")).collect();
        std::env::set_var("GTK_MODULES", kept.join(":"));
    }

    tracing_subscriber::fmt::init();

    // `HANDLES_COMMAND_LINE` forwards a second process's options (the
    // launcher actions below) to the running instance instead of only
    // activating it.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let worker = Rc::new(worker::Worker::new());
    // The dock badge's D-Bus work needs the worker's tokio reactor (the UI
    // thread's GLib context has none - see `launcher_entry`); hand the
    // handle over before any window code can publish a count.
    crate::launcher_entry::init(&worker);
    crate::tray::init(&worker);

    // The autostart entry launches `lookout --hidden`: build the window but
    // don't present it, so mail/calendar sync and the reminder loop run from
    // login without a window. Clicking the app icon or a notification's
    // action (`app.open-mailbox`, `app.open-event`, `app.raise-window`)
    // raises it.
    let hidden = Rc::new(Cell::new(false));
    {
        let hidden = hidden.clone();
        app.add_main_option(
            "hidden",
            b'h'.into(),
            glib::OptionFlags::NONE,
            glib::OptionArg::None,
            "Start without showing the main window",
            None,
        );
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

    // The desktop file's launcher actions (`lookout --compose` etc.), each
    // mapped to the app action of the same name that `build_window`
    // registers. A second process forwards these to the running instance.
    for (name, description) in LAUNCHER_ACTIONS {
        app.add_main_option(name, glib::Char::from(0), glib::OptionFlags::NONE, glib::OptionArg::None, description, None);
    }

    // Single window for the app's lifetime: an activation while one exists
    // (an app-icon click, a second process's D-Bus activation) just presents
    // it instead of building a duplicate.
    let window: Rc<RefCell<Option<adw::ApplicationWindow>>> = Rc::new(RefCell::new(None));
    {
        let window = window.clone();
        app.connect_command_line(move |app, command_line| {
            let options = command_line.options_dict();
            let requested: Vec<&str> = LAUNCHER_ACTIONS
                .iter()
                .map(|(name, _)| *name)
                .filter(|name| options.lookup_value(name, None).is_some())
                .collect();
            let hidden_requested = options.lookup_value("hidden", None).is_some() || command_line.arguments().iter().any(|arg| arg.to_str() == Some("--hidden"));
            // Activate builds the window on a cold start (honouring
            // `--hidden` through `handle-local-options` above) and presents
            // it otherwise - except for a repeated `--hidden` while running,
            // which should leave the window as it is.
            if window.borrow().is_none() || !hidden_requested || !requested.is_empty() {
                app.activate();
            }
            for name in requested {
                app.activate_action(name, None);
            }
            glib::ExitCode::SUCCESS
        });
    }
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
