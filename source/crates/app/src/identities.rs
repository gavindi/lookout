//! The "Manage identities…" dialog: add/edit/delete the sending identities
//! pinned to one account. Everything writes through to the shared
//! `AppConfig` (and `app_config::save`) on every change, and fires
//! `on_changed` so the caller (Config → Mail accounts, which also refreshes
//! any open composer's From dropdown) can re-render - the same write-through
//! pattern as the Manage-tags dialog.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use lookout_core::{AccountId, EmailAddress, Identity};

use crate::app_config::AppConfig;

/// Splits a comma-separated field into the address-spec strings it carries,
/// dropping anything that isn't a plausible address (empty fields, stray
/// text). Same tokenizer the composer's recipient chips use, so a
/// `"Surname, Given" <a@b.c>` display name stays one address.
fn parse_addresses(field: &str) -> Vec<EmailAddress> {
    crate::recipient_entry::parse_address_tokens(field)
        .into_iter()
        .filter(|token| crate::recipient_entry::is_plausible_address(token))
        .map(|token| EmailAddress::new(crate::recipient_entry::address_of(&token).to_string()))
        .collect()
}

/// Presents the modal manage-identities window, anchored to `anchor`'s
/// window when one exists (tests have no window). `on_changed` fires after
/// every persisted change so live views of the identity list can refresh.
pub fn show_manage_dialog(anchor: &gtk::Widget, app_config: Rc<RefCell<AppConfig>>, account_id: AccountId, on_changed: Rc<dyn Fn()>) {
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Manage identities").default_width(540).default_height(480);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).build();
    let scroller = gtk::ScrolledWindow::builder().child(&list).vexpand(true).build();

    // `rebuild` re-renders the identity rows; its per-row handlers call it
    // again after an edit or delete. The `Rc<RefCell<Box<dyn Fn()>>>` lets a
    // handler created while it runs still reach it. (The handlers clone the
    // `Rc`, which forms a reference cycle that lives until the dialog's
    // widgets drop - a bounded, one-off cost for a modal dialog.)
    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    let rebuild_handle = rebuild.clone();
    {
        let app_config = app_config.clone();
        let account_id = account_id.clone();
        let list = list.clone();
        let on_changed = on_changed.clone();
        let rebuild_handle = rebuild_handle.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let identities = app_config.borrow().identities_for(&account_id);
            if identities.is_empty() {
                let empty = gtk::Label::builder()
                    .label("No extra identities yet - messages send as this account's own address.")
                    .wrap(true)
                    .halign(gtk::Align::Start)
                    .css_classes(["dim-label"])
                    .margin_top(12)
                    .margin_bottom(12)
                    .build();
                list.append(&empty);
            }
            for identity in &identities {
                let row = gtk::ListBoxRow::new();
                let row_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .spacing(6)
                    .margin_top(6)
                    .margin_bottom(6)
                    .margin_start(10)
                    .margin_end(10)
                    .build();

                let name_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
                let name = gtk::Entry::builder().placeholder_text("Display name").hexpand(true).text(&identity.name).build();
                let email = gtk::Entry::builder().placeholder_text("name@example.com").hexpand(true).text(&identity.email).build();
                let delete = gtk::Button::from_icon_name("user-trash-symbolic");
                delete.set_tooltip_text(Some("Delete identity"));
                delete.add_css_class("flat");
                name_row.append(&name);
                name_row.append(&email);
                name_row.append(&delete);

                let reply_to = gtk::Entry::builder()
                    .placeholder_text("Reply-To (optional)")
                    .text(identity.reply_to.iter().map(|a| a.address.clone()).collect::<Vec<_>>().join(", "))
                    .build();
                let bcc = gtk::Entry::builder()
                    .placeholder_text("Blind copy (optional)")
                    .text(identity.bcc.iter().map(|a| a.address.clone()).collect::<Vec<_>>().join(", "))
                    .build();

                row_box.append(&name_row);
                row_box.append(&reply_to);
                row_box.append(&bcc);
                row.set_child(Some(&row_box));
                list.append(&row);

                let id = identity.id;
                let update = {
                    let app_config = app_config.clone();
                    let on_changed = on_changed.clone();
                    let name_entry = name.clone();
                    let email_entry = email.clone();
                    let reply_entry = reply_to.clone();
                    let bcc_entry = bcc.clone();
                    move || {
                        let name_text = name_entry.text().trim().to_string();
                        let email_text = email_entry.text().trim().to_string();
                        let email_valid = crate::recipient_entry::is_plausible_address(&email_text);
                        {
                            let mut config = app_config.borrow_mut();
                            let Some(identity) = config.identities.iter_mut().find(|i| i.id == id) else { return };
                            identity.name = name_text;
                            identity.reply_to = parse_addresses(&reply_entry.text());
                            identity.bcc = parse_addresses(&bcc_entry.text());
                            // An email mid-typing isn't persisted - the last
                            // valid value stays until the field is complete.
                            if email_valid {
                                identity.email = email_text;
                            }
                        }
                        crate::app_config::save(&app_config.borrow());
                        on_changed();
                    }
                };
                for entry in [&name, &email, &reply_to, &bcc] {
                    let update = update.clone();
                    entry.connect_changed(move |_| update());
                }

                let delete_config = app_config.clone();
                let delete_on_changed = on_changed.clone();
                let rebuild = rebuild_handle.clone();
                delete.connect_clicked(move |_| {
                    delete_config.borrow_mut().identities.retain(|i| i.id != id);
                    crate::app_config::save(&delete_config.borrow());
                    delete_on_changed();
                    (rebuild.borrow())();
                });
            }
        });
    }
    (rebuild.borrow())();

    // --- Add-an-identity row: name + email + Add button ---
    let add_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(8)
        .margin_start(10)
        .margin_end(10)
        .build();
    let add_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    let new_name = gtk::Entry::builder().placeholder_text("Display name").hexpand(true).build();
    let new_email = gtk::Entry::builder().placeholder_text("name@example.com").hexpand(true).build();
    let add_button = gtk::Button::with_label("Add");
    add_button.add_css_class("suggested-action");
    add_row.append(&new_name);
    add_row.append(&new_email);
    add_row.append(&add_button);

    let error_label = gtk::Label::builder().halign(gtk::Align::Start).css_classes(["error"]).visible(false).build();

    add_box.append(&add_row);
    add_box.append(&error_label);

    {
        let app_config = app_config.clone();
        let account_id = account_id.clone();
        let rebuild = rebuild_handle.clone();
        let on_changed = on_changed.clone();
        let new_email = new_email.clone();
        let new_email_for_activate = new_email.clone();
        let new_name = new_name.clone();
        let error_label = error_label.clone();
        let add = move || {
            let email_text = new_email.text().trim().to_string();
            let name_text = new_name.text().trim().to_string();
            if !crate::recipient_entry::is_plausible_address(&email_text) {
                error_label.set_label("That doesn't look like an email address.");
                error_label.set_visible(true);
                return;
            }
            {
                let config = app_config.borrow();
                if config.identities_for(&account_id).iter().any(|i| i.email.eq_ignore_ascii_case(&email_text)) {
                    error_label.set_label("An identity with this address already exists.");
                    error_label.set_visible(true);
                    return;
                }
            }
            error_label.set_visible(false);
            app_config.borrow_mut().identities.push(Identity::new(account_id.clone(), name_text, email_text));
            crate::app_config::save(&app_config.borrow());
            new_name.set_text("");
            new_email.set_text("");
            on_changed();
            (rebuild.borrow())();
        };
        let add_for_activate = add.clone();
        add_button.connect_clicked(move |_| add());
        new_email_for_activate.connect_activate(move |_| add_for_activate());
    }

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    root.append(&scroller);
    root.append(&add_box);
    let close_button = gtk::Button::builder().label("Done").css_classes(["suggested-action"]).margin_top(12).build();
    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| dialog.close());
    }
    root.append(&close_button);
    dialog.set_child(Some(&root));
    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_addresses_keeps_only_plausible_ones() {
        let addresses = parse_addresses("ada@example.com, \"Lovelace, Ada\" <ada2@example.com>, not-an-address, ");
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0].address, "ada@example.com");
        assert_eq!(addresses[1].address, "ada2@example.com");
    }

    #[test]
    fn parse_addresses_of_empty_or_garbage_field_is_empty() {
        assert!(parse_addresses("").is_empty());
        assert!(parse_addresses("hello world, ,").is_empty());
    }
}
