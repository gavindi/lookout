//! The "Search Folders…" dialog (Config → Mail, item 2.4 / M-19): manage the
//! saved searches shown as virtual folders in the folder pane. Edits write
//! through to `settings.json` (`AppConfig::search_folders`) and repaint the
//! folder pane via `UiState::folder_tree_refresh`.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::app_config::SearchFolder;
use crate::window::UiState;

/// Presents the modal Search Folders window, anchored to `anchor`'s window
/// when one exists.
pub fn show_manage_dialog(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>) {
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Search Folders").default_width(560).default_height(520);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let app_config = state.borrow().app_config.clone();
    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);
    let hint = gtk::Label::builder()
        .label("Saved searches appear as Search Folders in the folder pane. Use the search syntax, e.g. is:unread, is:flagged, from:ada@example.com, has:attachment, size:>5MB.")
        .wrap(true)
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();
    crate::privacy::mark_ui(&hint);

    // `rebuild` re-renders the list; each Remove calls it again through the
    // `Rc<RefCell<Box<dyn Fn()>>>` cycle bounded to the dialog.
    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    let rebuild_handle = rebuild.clone();
    let list_group = adw::PreferencesGroup::builder().title("Saved searches").build();
    *rebuild.borrow_mut() = Box::new({
        let app_config = app_config.clone();
        let list_group = list_group.clone();
        let rebuild_handle = rebuild_handle.clone();
        let state = state.clone();
        move || {
            while let Some(child) = list_group.first_child() {
                list_group.remove(&child);
            }
            let folders = app_config.borrow().search_folders.clone();
            if folders.is_empty() {
                let empty = gtk::Label::builder()
                    .label("No saved searches")
                    .xalign(0.0)
                    .css_classes(["dim-label", "caption"])
                    .margin_top(8)
                    .margin_bottom(8)
                    .build();
                crate::privacy::mark_ui(&empty);
                list_group.add(&empty);
                return;
            }
            for folder in folders {
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&folder.name))
                    .subtitle(glib::markup_escape_text(&folder.query))
                    .build();
                crate::privacy::mark_content(&row);
                let remove = gtk::Button::from_icon_name("user-trash-symbolic");
                remove.set_tooltip_text(Some("Remove this saved search"));
                remove.add_css_class("flat");
                row.add_suffix(&remove);
                list_group.add(&row);
                remove.connect_clicked({
                    let app_config = app_config.clone();
                    let state = state.clone();
                    let rebuild_handle = rebuild_handle.clone();
                    let id = folder.id.clone();
                    move |_| {
                        {
                            let mut config = app_config.borrow_mut();
                            config.search_folders.retain(|f| f.id != id);
                            crate::app_config::save(&config);
                        }
                        if let Some(refresh) = &state.borrow().folder_tree_refresh {
                            refresh();
                        }
                        (rebuild_handle.borrow())();
                    }
                });
            }
        }
    });

    // Add form: a name and a query.
    let add_group = adw::PreferencesGroup::builder().title("Add a saved search").build();
    let name_entry = adw::EntryRow::builder().title("Name").build();
    let query_entry = adw::EntryRow::builder().title("Search query").build();
    let add_button = gtk::Button::with_label("Add");
    add_button.add_css_class("suggested-action");
    add_button.set_halign(gtk::Align::End);
    add_group.add(&name_entry);
    add_group.add(&query_entry);
    add_group.add(&add_button);
    add_button.connect_clicked({
        let app_config = app_config.clone();
        let state = state.clone();
        let rebuild_handle = rebuild_handle.clone();
        let name_entry = name_entry.clone();
        let query_entry = query_entry.clone();
        move |_| {
            let name = name_entry.text().trim().to_string();
            let query = query_entry.text().trim().to_string();
            if name.is_empty() || query.is_empty() {
                return;
            }
            {
                let mut config = app_config.borrow_mut();
                config.search_folders.push(SearchFolder {
                    id: format!("search:{}", uuid::Uuid::new_v4()),
                    name,
                    query,
                });
                crate::app_config::save(&config);
            }
            name_entry.set_text("");
            query_entry.set_text("");
            if let Some(refresh) = &state.borrow().folder_tree_refresh {
                refresh();
            }
            (rebuild_handle.borrow())();
        }
    });

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    content.append(&hint);
    content.append(&list_group);
    content.append(&add_group);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&content)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    root.append(&scroller);
    let close_button = gtk::Button::builder().label("Done").css_classes(["suggested-action"]).margin_top(12).build();
    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| dialog.close());
    }
    root.append(&close_button);
    dialog.set_child(Some(&root));
    (rebuild.borrow())();
    dialog.present();
}
