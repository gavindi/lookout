//! GTK4/libadwaita application entry point.

mod compose;
mod folder_tree;
mod goa_credentials;
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
