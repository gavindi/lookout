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
use lookout_graph::DirectoryPerson;

use crate::contacts_view::dedupe_addresses;
use crate::directory::DirectoryLookup;
use crate::recipient_entry::Suggestion;
use crate::window::UiState;

/// Opens the picker over the given anchor widget's window, handing whatever
/// the user checked to the given callback. Threaded into `build_compose_view`
/// so the To button can open it without the composer needing to know about
/// `UiState` itself.
pub type ContactPickerOpener = Rc<dyn Fn(gtk::Widget, Rc<dyn Fn(Vec<Suggestion>)>)>;

/// One pickable row: a stored address, a contact list, or a read-only
/// directory/GAL hit (item 4.7.7). A directory row carries no stored contact
/// behind it, so it can only be picked - never edited - and picking one adds
/// its address as a normal recipient.
#[derive(Clone)]
enum Item {
    Address(EmailAddress),
    List(ContactList),
    Directory(DirectoryPerson),
}

impl Item {
    fn label(&self) -> String {
        match self {
            Item::Address(address) => address.display_label().to_string(),
            Item::List(list) => list.name.clone(),
            Item::Directory(person) => directory_name(person),
        }
    }

    /// The stable selection key: a lowercased address, `list:<lowercased
    /// name>`, or `dir:<lowercased address>` so the three can never collide.
    fn key(&self) -> String {
        match self {
            Item::Address(address) => address.address.to_lowercase(),
            Item::List(list) => format!("list:{}", list.name.to_lowercase()),
            Item::Directory(person) => format!("dir:{}", person.address.trim().to_lowercase()),
        }
    }

    fn matches(&self, filter: &str) -> bool {
        filter.is_empty()
            || match self {
                Item::Address(address) => address.display_label().to_lowercase().contains(filter) || address.address.to_lowercase().contains(filter),
                Item::List(list) => list.name.to_lowercase().contains(filter),
                Item::Directory(person) => {
                    person.address.to_lowercase().contains(filter)
                        || person.name.as_deref().is_some_and(|name| name.to_lowercase().contains(filter))
                        || person.company.as_deref().is_some_and(|company| company.to_lowercase().contains(filter))
                        || person.title.as_deref().is_some_and(|title| title.to_lowercase().contains(filter))
                }
            }
    }

    /// The dim subtitle under the label, or `None` when the label says it all.
    fn subtitle(&self) -> Option<String> {
        match self {
            // A contact with no separate display name would otherwise show its
            // address twice.
            Item::Address(address) => (address.display_label() != address.address).then(|| address.address.clone()),
            Item::List(list) => Some(format!("Contact list · {} people", list.members.len())),
            Item::Directory(person) => Some(directory_subtitle(person)),
        }
    }

    fn into_suggestion(self) -> Suggestion {
        match self {
            Item::Address(address) => Suggestion::Address(address),
            Item::List(list) => Suggestion::List(list),
            Item::Directory(person) => Suggestion::Directory(person),
        }
    }
}

/// A directory hit's row label: the display name when there is one, else its
/// address.
fn directory_name(person: &DirectoryPerson) -> String {
    person
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| person.address.trim().to_string())
}

/// A directory hit's row caption, flagging it as a directory (not a stored
/// contact) and adding the company or job title when the entry carries one.
fn directory_subtitle(person: &DirectoryPerson) -> String {
    let detail = person
        .company
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| person.title.as_deref().map(str::trim).filter(|value| !value.is_empty()));
    match detail {
        Some(detail) => format!("Directory · {detail}"),
        None => "Directory".to_string(),
    }
}

/// Presents the modal "Add Recipients" picker, `transient_for` whichever
/// window `anchor` currently lives in (the main window or a popped-out
/// composer). `on_add` fires once, with every checked contact, when "Add" is
/// clicked; Cancel or closing the window without adding calls nothing.
///
/// `directory` is the composer's directory lookup (item 4.7.7), or `None` when
/// no directory is configured. Once the search box has text it is queried and
/// its hits appear as read-only rows under a **Directory** header; picking one
/// adds its address like any other contact.
pub fn show_contact_picker(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>, directory: Option<DirectoryLookup>, on_add: Rc<dyn Fn(Vec<Suggestion>)>) {
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

    // The directory hits currently shown, filled in asynchronously by the
    // lookup below (item 4.7.7). Rebuilt row-by-row like `items`, but read-only
    // and under their own header.
    let directory_items: Rc<RefCell<Vec<Item>>> = Rc::new(RefCell::new(Vec::new()));

    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    {
        let items = items.clone();
        let directory_items = directory_items.clone();
        let list = list.clone();
        let selected = selected.clone();
        let search = search.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let filter = search.text().to_string().to_lowercase();
            // Owned clones, so the directory list isn't borrowed across the
            // rebuild (its `RefCell` may be written by an in-flight lookup).
            let matches: Vec<Item> = items.iter().filter(|item| item.matches(&filter)).cloned().collect();
            let directory_matches: Vec<Item> = directory_items.borrow().iter().filter(|item| item.matches(&filter)).cloned().collect();

            if matches.is_empty() && directory_matches.is_empty() {
                let empty = gtk::Label::builder()
                    .label(if items.is_empty() && directory_items.borrow().is_empty() {
                        "No contacts yet"
                    } else {
                        "No contacts found"
                    })
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

            for candidate in &matches {
                append_picker_row(&list, candidate, &selected);
            }
            if !directory_matches.is_empty() {
                append_section_header(&list, "Directory");
                for candidate in &directory_matches {
                    append_picker_row(&list, candidate, &selected);
                }
            }
        });
    }
    (rebuild.borrow())();
    {
        let rebuild = rebuild.clone();
        let directory_items = directory_items.clone();
        search.connect_search_changed(move |entry| {
            (rebuild.borrow())();
            let Some(directory) = directory.clone() else { return };
            let query = entry.text().trim().to_string();
            if query.is_empty() {
                // Nothing typed: drop the last query's hits rather than leave
                // them under an empty search box.
                directory_items.borrow_mut().clear();
                (rebuild.borrow())();
                return;
            }
            // The lookup answers from its cache on this call or, for a fresh
            // prefix, after its debounce; its own keystroke generation guard
            // means only the newest query delivers, so a stale reply can't
            // overwrite the rows. The text check is a second guard against a
            // cached reply landing after a newer keystroke.
            let entry = entry.clone();
            let issued = query.clone();
            directory(
                &query,
                Rc::new({
                    let directory_items = directory_items.clone();
                    let rebuild = rebuild.clone();
                    move |suggestions| {
                        if entry.text().trim() != issued.as_str() {
                            return;
                        }
                        let hits: Vec<Item> = suggestions
                            .into_iter()
                            .filter_map(|suggestion| match suggestion {
                                Suggestion::Directory(person) => Some(Item::Directory(person)),
                                _ => None,
                            })
                            .collect();
                        *directory_items.borrow_mut() = hits;
                        (rebuild.borrow())();
                    }
                }),
            );
        });
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
        let directory_items = directory_items.clone();
        add_button.connect_clicked(move |_| {
            let directory_items = directory_items.borrow();
            let picked: Vec<Suggestion> = items
                .iter()
                .chain(directory_items.iter())
                .filter(|item| selected.borrow().contains(&item.key()))
                .cloned()
                .map(Item::into_suggestion)
                .collect();
            drop(directory_items);
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

/// Appends one pickable row (checkbox + label + optional subtitle) to the
/// picker's list, wired to the shared `selected` set by the item's stable key.
fn append_picker_row(list: &gtk::ListBox, candidate: &Item, selected: &Rc<RefCell<HashSet<String>>>) {
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

    let name = gtk::Label::builder().label(candidate.label()).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let text_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text_box.append(&name);
    if let Some(subtitle) = candidate.subtitle() {
        let subtitle = gtk::Label::builder()
            .label(&subtitle)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        text_box.append(&subtitle);
    }

    row_box.append(&checkbox);
    row_box.append(&text_box);
    row.set_child(Some(&row_box));
    list.append(&row);
}

/// Appends a non-selectable section header (currently only "Directory") to the
/// picker's list.
fn append_section_header(list: &gtk::ListBox, label: &str) {
    let header = gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .css_classes(["heading"])
        .margin_top(8)
        .margin_bottom(2)
        .margin_start(10)
        .margin_end(10)
        .build();
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&header));
    row.set_selectable(false);
    row.set_activatable(false);
    list.append(&row);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person() -> DirectoryPerson {
        DirectoryPerson {
            id: Some("graph-id".to_string()),
            name: Some("Grace Hopper".to_string()),
            address: "grace@example.com".to_string(),
            title: Some("Admiral".to_string()),
            company: Some("US Navy".to_string()),
        }
    }

    #[test]
    fn a_directory_item_labels_keys_and_matches_on_every_field() {
        let item = Item::Directory(person());
        assert_eq!(item.label(), "Grace Hopper");
        assert_eq!(item.key(), "dir:grace@example.com");
        assert_eq!(item.subtitle().as_deref(), Some("Directory · US Navy"));
        assert!(item.matches(""));
        assert!(item.matches("grace"));
        assert!(item.matches("hopper"));
        assert!(item.matches("navy"));
        assert!(item.matches("admiral"));
        assert!(!item.matches("zzz"));
    }

    #[test]
    fn a_directory_item_falls_back_to_the_address_and_falls_back_for_its_caption() {
        let nameless = DirectoryPerson {
            name: Some("   ".to_string()),
            title: None,
            company: None,
            ..person()
        };
        let item = Item::Directory(nameless);
        assert_eq!(item.label(), "grace@example.com");
        assert_eq!(item.subtitle().as_deref(), Some("Directory"));
        // The title stands in when there is no company to show.
        let titled = DirectoryPerson { company: None, ..person() };
        assert_eq!(Item::Directory(titled).subtitle().as_deref(), Some("Directory · Admiral"));
    }

    #[test]
    fn a_directory_item_commits_as_a_directory_suggestion() {
        let item = Item::Directory(person());
        assert_eq!(item.into_suggestion(), Suggestion::Directory(person()));
    }
}
