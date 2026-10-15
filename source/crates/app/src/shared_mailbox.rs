//! Add and remove shared mailboxes (OUTLOOK_PARITY.md item 5.6 / M-42).
//!
//! A shared mailbox is a second Exchange Online mailbox a Microsoft 365 user
//! has been granted Full Access to. Lookout adds it by address against a parent
//! Microsoft 365 account, and connects it as its own mail account borrowing
//! that parent's OAuth token (see `window/connect.rs`'s
//! `connect_shared_mailboxes`). The list lives in `settings.json`
//! (`AppConfig::shared_mailboxes`).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use lookout_core::AccountId;

use crate::app_config::{AppConfig, SharedMailbox, SHARED_MAILBOX_ID_PREFIX};

/// One Microsoft 365 account a shared mailbox can be added to, for the
/// dialog's parent picker.
#[derive(Clone)]
pub struct SharedMailboxParent {
    pub account_id: AccountId,
    pub label: String,
}

/// The dialog's list-rebuild slot (the rebuild and the row Remove handlers are
/// mutually referential - the `ManageRebuild` idiom).
type RebuildSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// Opens the "Shared mailboxes" dialog.
pub fn show_manage_shared_mailboxes(anchor: &gtk::Widget, app_config: Rc<RefCell<AppConfig>>, parents: Vec<SharedMailboxParent>, on_changed: Rc<dyn Fn()>) {
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Shared mailboxes").default_width(520).default_height(480);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    // --- Add section: address + a parent picker (auto-selected when there's
    // exactly one Microsoft 365 account).
    let address_entry = adw::EntryRow::builder().title("Shared mailbox address").build();
    let parent_dropdown = gtk::DropDown::builder().build();
    let parent_labels: Vec<&str> = parents.iter().map(|p| p.label.as_str()).collect();
    parent_dropdown.set_model(Some(&gtk::StringList::new(&parent_labels)));
    parent_dropdown.set_selected(0);
    parent_dropdown.set_tooltip_text(Some("Microsoft 365 account"));
    parent_dropdown.set_sensitive(parents.len() > 1);
    let add_button = gtk::Button::with_label("Add");
    add_button.add_css_class("suggested-action");
    add_button.set_halign(gtk::Align::End);
    let add_group = adw::PreferencesGroup::builder().title("Add a shared mailbox").build();
    add_group.add(&address_entry);
    add_group.add(&parent_dropdown);
    add_group.add(&add_button);
    if parents.is_empty() {
        add_group.set_description(Some("Add and enable a Microsoft 365 account first - a shared mailbox borrows its Outlook access"));
        address_entry.set_sensitive(false);
        parent_dropdown.set_sensitive(false);
        add_button.set_sensitive(false);
    }

    // --- Existing shared mailboxes, each with a Remove.
    let list_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let list_group = adw::PreferencesGroup::builder().title("Your shared mailboxes").build();
    list_group.add(&list_box);

    let rebuild: RebuildSlot = Rc::new(RefCell::new(None));
    *rebuild.borrow_mut() = Some(Rc::new({
        let list_box = list_box.clone();
        let app_config = app_config.clone();
        let on_changed = on_changed.clone();
        let rebuild = rebuild.clone();
        move || {
            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            let mailboxes = app_config.borrow().shared_mailboxes.clone();
            if mailboxes.is_empty() {
                list_box.append(
                    &gtk::Label::builder()
                        .label("No shared mailboxes yet")
                        .xalign(0.0)
                        .css_classes(["dim-label", "caption"])
                        .build(),
                );
                return;
            }
            for mailbox in mailboxes {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let text = gtk::Label::builder()
                    .label(&mailbox.address)
                    .xalign(0.0)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                let remove = gtk::Button::from_icon_name("user-trash-symbolic");
                remove.add_css_class("flat");
                remove.set_tooltip_text(Some("Remove shared mailbox"));
                remove.connect_clicked({
                    let app_config = app_config.clone();
                    let on_changed = on_changed.clone();
                    let rebuild = rebuild.clone();
                    let id = mailbox.id.clone();
                    let row = row.clone();
                    move |_| {
                        {
                            let mut config = app_config.borrow_mut();
                            config.shared_mailboxes.retain(|m| m.id != id);
                            crate::app_config::save(&config);
                        }
                        row.set_visible(false);
                        on_changed();
                        if let Some(rebuild) = rebuild.borrow().clone() {
                            rebuild();
                        }
                    }
                });
                row.append(&text);
                row.append(&remove);
                list_box.append(&row);
            }
        }
    }));

    add_button.connect_clicked({
        let address_entry = address_entry.clone();
        let parent_dropdown = parent_dropdown.clone();
        let app_config = app_config.clone();
        let on_changed = on_changed.clone();
        let rebuild = rebuild.clone();
        let parents = parents.clone();
        move |_| {
            let address = address_entry.text().trim().to_string();
            if address.is_empty() {
                return;
            }
            let Some(parent) = parents.get(parent_dropdown.selected() as usize) else {
                return;
            };
            let display_name = address.split('@').next().map(str::to_string).unwrap_or_else(|| address.clone());
            let mailbox = SharedMailbox {
                id: format!("{SHARED_MAILBOX_ID_PREFIX}{}", uuid::Uuid::new_v4()),
                parent_account: parent.account_id.clone(),
                address,
                display_name,
                owner_id: None,
            };
            {
                let mut config = app_config.borrow_mut();
                config.shared_mailboxes.push(mailbox);
                crate::app_config::save(&config);
            }
            address_entry.set_text("");
            on_changed();
            if let Some(rebuild) = rebuild.borrow().clone() {
                rebuild();
            }
        }
    });

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    content.append(&add_group);
    content.append(&list_group);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&content)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    dialog.set_child(Some(&scroller));
    if let Some(rebuild) = rebuild.borrow().clone() {
        rebuild();
    }
    dialog.present();
}
