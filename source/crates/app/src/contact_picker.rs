//! The compose window's "To" button: a modal picker over every known
//! contact (CardDAV address books across every connected account) and every
//! contact list, with checkbox multi-select and an "Add" button that hands
//! the picks back to the caller. Modeled on
//! `identities.rs::show_manage_dialog`'s dialog skeleton and the People
//! screen's own checkbox-row pattern (`contacts_view.rs`), stripped down to
//! just search-and-pick.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use lookout_core::{ContactList, EmailAddress};

use crate::contacts_view::dedupe_addresses;
use crate::recipient_entry::Suggestion;
use crate::window::UiState;

/// Opens the picker over the given anchor widget's window, handing whatever
/// the user checked to the given callback. Threaded into `build_compose_view`
/// so the To button can open it without the composer needing to know about
/// `UiState` itself.
pub type ContactPickerOpener = Rc<dyn Fn(gtk::Widget, Rc<dyn Fn(Vec<Suggestion>)>)>;

/// One pickable row: a single address, or a contact list.
#[derive(Clone)]
enum Item {
    Address(EmailAddress),
    List(ContactList),
}

impl Item {
    fn label(&self) -> String {
        match self {
            Item::Address(address) => address.display_label().to_string(),
            Item::List(list) => list.name.clone(),
        }
    }

    /// The stable selection key: a lowercased address, or `list:<lowercased
    /// name>` so a list and an address can never collide.
    fn key(&self) -> String {
        match self {
            Item::Address(address) => address.address.to_lowercase(),
            Item::List(list) => format!("list:{}", list.name.to_lowercase()),
        }
    }

    fn matches(&self, filter: &str) -> bool {
        filter.is_empty()
            || match self {
                Item::Address(address) => {
                    address.display_label().to_lowercase().contains(filter) || address.address.to_lowercase().contains(filter)
                }
                Item::List(list) => list.name.to_lowercase().contains(filter),
            }
    }

    fn into_suggestion(self) -> Suggestion {
        match self {
            Item::Address(address) => Suggestion::Address(address),
            Item::List(list) => Suggestion::List(list),
        }
    }
}

/// Presents the modal "Add Recipients" picker, `transient_for` whichever
/// window `anchor` currently lives in (the main window or a popped-out
/// composer). `on_add` fires once, with every checked contact, when "Add" is
/// clicked; Cancel or closing the window without adding calls nothing.
pub fn show_contact_picker(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>, on_add: Rc<dyn Fn(Vec<Suggestion>)>) {
    let mut contacts: Vec<EmailAddress> = state.borrow().contacts_by_account.values().flat_map(|snapshot| snapshot.suggestions.clone()).collect();
    contacts = dedupe_addresses(contacts, usize::MAX);
    contacts.sort_by_key(|a| a.display_label().to_lowercase());

    let lists: Vec<ContactList> = {
        let mut seen = HashSet::new();
        let mut lists: Vec<ContactList> = state
            .borrow()
            .contacts_by_account
            .values()
            .flat_map(|snapshot| snapshot.contact_lists.clone())
            .filter(|list| !list.name.trim().is_empty() && seen.insert(list.name.to_lowercase()))
            .collect();
        lists.sort_by_key(|list| list.name.to_lowercase());
        lists
    };

    let mut items: Vec<Item> = contacts.into_iter().map(Item::Address).collect();
    items.extend(lists.into_iter().map(Item::List));

    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Add Recipients").default_width(420).default_height(480);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search contacts")
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .build();

    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .build();

    // Keyed by the item's stable key rather than list position, so a check
    // survives the list being rebuilt (filtered) out from under it.
    let selected: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));

    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    {
        let items = items.clone();
        let list = list.clone();
        let selected = selected.clone();
        let search = search.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let filter = search.text().to_string().to_lowercase();
            let matches: Vec<&Item> = items.iter().filter(|item| item.matches(&filter)).collect();

            if matches.is_empty() {
                let empty = gtk::Label::builder()
                    .label(if items.is_empty() { "No contacts yet" } else { "No contacts found" })
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
                let key = candidate.key();
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

                let name = gtk::Label::builder()
                    .label(candidate.label())
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                let text_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .spacing(2)
                    .hexpand(true)
                    .valign(gtk::Align::Center)
                    .build();
                text_box.append(&name);
                match candidate {
                    // A contact with no separate display name would otherwise
                    // show its address twice.
                    Item::Address(address) if address.display_label() != address.address => {
                        let subtitle = gtk::Label::builder()
                            .label(&address.address)
                            .xalign(0.0)
                            .ellipsize(gtk::pango::EllipsizeMode::End)
                            .css_classes(["dim-label", "caption"])
                            .build();
                        text_box.append(&subtitle);
                    }
                    Item::List(list) => {
                        let subtitle = gtk::Label::builder()
                            .label(format!("Contact list · {} people", list.members.len()))
                            .xalign(0.0)
                            .ellipsize(gtk::pango::EllipsizeMode::End)
                            .css_classes(["dim-label", "caption"])
                            .build();
                        text_box.append(&subtitle);
                    }
                    Item::Address(_) => {}
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
        let items = items.clone();
        add_button.connect_clicked(move |_| {
            let picked: Vec<Suggestion> = items
                .iter()
                .filter(|item| selected.borrow().contains(&item.key()))
                .cloned()
                .map(Item::into_suggestion)
                .collect();
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
