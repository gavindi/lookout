//! The "My Templates" editor (item 2.10 / C-17): Config → Mail → "My
//! Templates" opens a modal rich-text editor (the same contenteditable WebKit
//! editor the composer and the signature editor use), and saving writes
//! through to the shared `AppConfig` (and `app_config::save`) and fires
//! `on_changed` so the settings screen's template list can refresh - the same
//! write-through pattern as the signature editor. A template is a reusable
//! snippet inserted into the composer from its "My Templates" menu.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use lookout_core::MailTemplate;

use crate::app_config::AppConfig;

/// Presents the modal template editor, anchored to `anchor`'s window when one
/// exists (tests have no window). `existing` prefills the editor (the edit
/// path); `None` starts a blank template (the add path). `on_changed` fires
/// after every persisted change so live views of the template list can
/// refresh.
pub fn show_editor_dialog(anchor: &gtk::Widget, app_config: Rc<RefCell<AppConfig>>, existing: Option<MailTemplate>, on_changed: Rc<dyn Fn()>) {
    let dialog = {
        let mut builder = gtk::Window::builder()
            .modal(true)
            .title(if existing.is_some() { "Edit template" } else { "New template" })
            .default_width(720)
            .default_height(560);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let name_entry = gtk::Entry::builder()
        .placeholder_text("Name (e.g. Status update, Meeting notes)")
        .text(existing.as_ref().map(|t| t.name.as_str()).unwrap_or(""))
        .build();
    let (web_view, format_toolbar) = crate::compose::build_rich_editor(existing.as_ref().map(|t| t.html.clone()).unwrap_or_default());

    let toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    toolbar.append(&format_toolbar);
    let rich_scroller = gtk::ScrolledWindow::builder().child(&*web_view).hexpand(true).vexpand(true).build();

    let save_button = gtk::Button::with_label("Save");
    save_button.add_css_class("suggested-action");
    let cancel_button = gtk::Button::with_label("Cancel");
    let button_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).halign(gtk::Align::End).build();
    button_row.append(&cancel_button);
    button_row.append(&save_button);

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    root.append(&name_entry);
    root.append(&toolbar);
    root.append(&rich_scroller);
    root.append(&button_row);
    dialog.set_child(Some(&root));

    // Save stays disabled until a name is entered - the template menu lists
    // entries by name, so an unnamed template would be unreachable.
    save_button.set_sensitive(!name_entry.text().is_empty());
    {
        let save_button = save_button.clone();
        name_entry.connect_changed(move |entry| save_button.set_sensitive(!entry.text().is_empty()));
    }
    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }

    let edit_id = existing.as_ref().map(|t| t.id);
    let save = {
        let dialog = dialog.clone();
        let app_config = app_config.clone();
        let on_changed = on_changed.clone();
        let name_entry = name_entry.clone();
        let web_view = web_view.clone();
        move || {
            let name = name_entry.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            let dialog = dialog.clone();
            let app_config = app_config.clone();
            let on_changed = on_changed.clone();
            let web_view = web_view.clone();
            // `read_content` must stay async (it awaits the WebView's JS
            // evaluation); see `compose::read_content` for why blocking is
            // unsafe inside a glib-spawned future.
            gtk::glib::spawn_future_local(async move {
                let Some(content) = crate::compose::read_content(&web_view).await else { return };
                let template = MailTemplate::new(name, crate::signatures::embed_images(&content.html, &content.images), content.text);
                {
                    let mut config = app_config.borrow_mut();
                    if let Some(id) = edit_id {
                        config.upsert_template(MailTemplate { id, ..template });
                    } else {
                        config.upsert_template(template);
                    }
                }
                crate::app_config::save(&app_config.borrow());
                on_changed();
                dialog.close();
            });
        }
    };
    let save_for_activate = save.clone();
    save_button.connect_clicked(move |_| save());
    name_entry.connect_activate(move |_| save_for_activate());

    dialog.present();
}
