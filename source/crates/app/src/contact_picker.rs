//! The compose window's "To" button: a modal picker over every known
//! contact (CardDAV address books across every connected account), with
//! checkbox multi-select and an "Add" button that hands the picks back to
//! the caller. Modeled on `identities.rs::show_manage_dialog`'s dialog
//! skeleton and the People screen's own checkbox-row pattern
//! (`contacts_view.rs`), stripped down to just search-and-pick.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use lookout_core::EmailAddress;

use crate::contacts_view::dedupe_addresses;
use crate::window::UiState;

/// Opens the picker over the given anchor widget's window, handing whatever
/// the user checked to the given callback. Threaded into `build_compose_view`
/// so the To button can open it without the composer needing to know about
/// `UiState` itself.
pub type ContactPickerOpener = Rc<dyn Fn(gtk::Widget, Rc<dyn Fn(Vec<EmailAddress>)>)>;

/// Presents the modal "Add Recipients" picker, `transient_for` whichever
/// window `anchor` currently lives in (the main window or a popped-out
/// composer). `on_add` fires once, with every checked contact, when "Add" is
/// clicked; Cancel or closing the window without adding calls nothing.
pub fn show_contact_picker(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>, on_add: Rc<dyn Fn(Vec<EmailAddress>)>) {
    let mut contacts: Vec<EmailAddress> = state.borrow().contacts_by_account.values().flat_map(|snapshot| snapshot.suggestions.clone()).collect();
    contacts = dedupe_addresses(contacts, usize::MAX);
    contacts.sort_by_key(|a| a.display_label().to_lowercase());

    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Add Recipients").default_width(420).default_height(480);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let search = gtk::SearchEntry::builder().placeholder_text("Search contacts").margin_start(12).margin_end(12).margin_top(12).build();

    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).build();
    let scroller = gtk::ScrolledWindow::builder().child(&list).vexpand(true).margin_start(12).margin_end(12).margin_top(6).build();

    // Keyed by lowercased address rather than list position, so a check
    // survives the list being rebuilt (filtered) out from under it.
    let selected: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));

    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    {
        let contacts = contacts.clone();
        let list = list.clone();
        let selected = selected.clone();
        let search = search.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let filter = search.text().to_string().to_lowercase();
            let matches: Vec<&EmailAddress> = contacts
                .iter()
                .filter(|c| filter.is_empty() || c.display_label().to_lowercase().contains(&filter) || c.address.to_lowercase().contains(&filter))
                .collect();

            if matches.is_empty() {
                let empty = gtk::Label::builder()
                    .label(if contacts.is_empty() { "No contacts yet" } else { "No contacts found" })
                    .wrap(true)
                    .halign(gtk::Align::Start)
                    .css_classes(["dim-label"])
                    .margin_top(12)
                    .margin_bottom(12)
                    .margin_start(10)
                    .margin_end(10)
                    .build();
                list.append(&empty);
                return;
            }

            for candidate in matches {
                let row = gtk::ListBoxRow::new();
                let row_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(8)
                    .margin_top(6)
                    .margin_bottom(6)
                    .margin_start(10)
                    .margin_end(10)
                    .build();

                let checkbox = gtk::CheckButton::builder().valign(gtk::Align::Center).build();
                let key = candidate.address.to_lowercase();
                checkbox.set_active(selected.borrow().contains(&key));
                {
                    let selected = selected.clone();
                    let key = key.clone();
                    checkbox.connect_toggled(move |btn| {
                        if btn.is_active() {
                            selected.borrow_mut().insert(key.clone());
                        } else {
                            selected.borrow_mut().remove(&key);
                        }
                    });
                }

                let name = gtk::Label::builder().label(candidate.display_label()).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
                let text_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).valign(gtk::Align::Center).build();
                text_box.append(&name);
                // A contact with no separate display name would otherwise
                // show its address twice.
                if candidate.display_label() != candidate.address {
                    let address = gtk::Label::builder()
                        .label(&candidate.address)
                        .xalign(0.0)
                        .ellipsize(gtk::pango::EllipsizeMode::End)
                        .css_classes(["dim-label", "caption"])
                        .build();
                    text_box.append(&address);
                }

                row_box.append(&checkbox);
                row_box.append(&text_box);
                row.set_child(Some(&row_box));
                list.append(&row);
            }
        });
    }
    (rebuild.borrow())();
    {
        let rebuild = rebuild.clone();
        search.connect_search_changed(move |_| (rebuild.borrow())());
    }

    let cancel_button = gtk::Button::builder().label("Cancel").build();
    let add_button = gtk::Button::builder().label("Add").css_classes(["suggested-action"]).build();
    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }
    {
        let dialog = dialog.clone();
        let contacts = contacts.clone();
        add_button.connect_clicked(move |_| {
            let picked: Vec<EmailAddress> = contacts.iter().filter(|c| selected.borrow().contains(&c.address.to_lowercase())).cloned().collect();
            dialog.close();
            if !picked.is_empty() {
                on_add(picked);
            }
        });
    }
    let action_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    action_row.append(&cancel_button);
    action_row.append(&add_button);

    let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    root.append(&search);
    root.append(&scroller);
    root.append(&action_row);
    dialog.set_child(Some(&root));
    dialog.present();
}
