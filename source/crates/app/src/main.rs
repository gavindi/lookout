//! GTK4/libadwaita application entry point.

mod calendar_colors;
mod calendar_view;
mod compose;
mod config_view;
mod folder_tree;
mod goa_calendar_credentials;
mod goa_credentials;
mod last_view;
mod message_header;
mod message_list;
mod microsoft_oauth;
mod window;
mod worker;

use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

const APP_ID: &str = "io.github.gavindi.Lookout";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt::init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    let worker = Rc::new(worker::Worker::new());

    app.connect_activate(move |app| {
        let win = window::build_window(app, worker.clone());
        win.present();
    });

    app.run()
}
