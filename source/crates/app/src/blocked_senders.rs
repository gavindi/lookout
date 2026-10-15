//! The "Blocked senders…" dialog (Config → Mail → Junk email, item 2.9 /
//! M-17): add/remove the sender entries whose mail goes straight to Junk.
//! Everything writes through to the UI-state database (`ui_state_db`) and the
//! in-memory `UiState::blocked_senders` mirror on every change, so the
//! client-rule arrival pass (`rules::enforce_blocked_senders`) picks the
//! change up immediately - the same write-through pattern as the Safe senders
//! dialog. Entries are keyed per receiving account, so each account gets its
//! own section and add row. Adding an address that is on the safe list removes
//! it from there (a sender is never both safe and blocked).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::AccountId;

use crate::util::account_label;
use crate::window::UiState;

/// Presents the modal blocked-senders window, anchored to `anchor`'s window
/// when one exists (tests have no window).
pub fn show_manage_dialog(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>) {
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Blocked senders").default_width(560).default_height(480);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);
    let hint = gtk::Label::builder()
        .label("Mail from blocked senders goes straight to Junk. Enter an address (name@example.com) or a whole domain (@example.com).")
        .wrap(true)
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();

    // `rebuild` re-renders the per-account sections; its per-row handlers call
    // it again after an add/remove, reaching it through the `Rc<RefCell<Box<
    // dyn Fn()>>>` (a reference cycle bounded to the dialog's lifetime, as in
    // the Safe senders dialog).
    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    let rebuild_handle = rebuild.clone();
    let groups: Rc<RefCell<Vec<adw::PreferencesGroup>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let state = state.clone();
        let page = page.clone();
        let rebuild_handle = rebuild_handle.clone();
        let groups = groups.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            for group in groups.borrow().iter() {
                page.remove(group);
            }
            groups.borrow_mut().clear();
            let (entries, accounts) = {
                let st = state.borrow();
                let entries: Vec<(AccountId, String)> = st.blocked_senders.iter().cloned().collect();
                let mut accounts: Vec<(AccountId, String)> = st
                    .accounts
                    .iter()
                    .map(|(account, handle)| (account.clone(), account_label(account, &handle.display_name, &handle.email)))
                    .collect();
                accounts.sort_by_key(|a| a.1.to_lowercase());
                (entries, accounts)
            };
            if accounts.is_empty() {
                let group = adw::PreferencesGroup::new();
                let empty = gtk::Label::builder()
                    .label("No mail accounts connected yet - blocked senders are remembered per account.")
                    .wrap(true)
                    .halign(gtk::Align::Start)
                    .css_classes(["dim-label"])
                    .margin_top(12)
                    .margin_bottom(12)
                    .build();
                group.add(&empty);
                page.add(&group);
                groups.borrow_mut().push(group);
                return;
            }
            for (account, label) in accounts {
                let group = adw::PreferencesGroup::builder().title(glib::markup_escape_text(&label)).build();
                groups.borrow_mut().push(group.clone());
                let mut account_entries: Vec<String> = entries.iter().filter(|(a, _)| *a == account).map(|(_, entry)| entry.clone()).collect();
                account_entries.sort();
                for entry in &account_entries {
                    let row = adw::ActionRow::builder().title(glib::markup_escape_text(entry)).build();
                    let remove = gtk::Button::from_icon_name("user-trash-symbolic");
                    remove.set_tooltip_text(Some("Remove this sender"));
                    remove.add_css_class("flat");
                    row.add_suffix(&remove);
                    group.add(&row);
                    remove.connect_clicked({
                        let state = state.clone();
                        let account = account.clone();
                        let entry = entry.clone();
                        let rebuild_handle = rebuild_handle.clone();
                        move |_| {
                            state.borrow_mut().blocked_senders.remove(&(account.clone(), entry.clone()));
                            if let Some(db) = &state.borrow().ui_db {
                                let _ = db.remove_blocked_sender(&account, &entry);
                            }
                            (rebuild_handle.borrow())();
                        }
                    });
                }
                // Per-account add row: entry field + Add.
                let add_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(6)
                    .margin_top(6)
                    .margin_bottom(6)
                    .build();
                let entry_field = gtk::Entry::builder().placeholder_text("name@example.com or @example.com").hexpand(true).build();
                let error_label = gtk::Label::builder().label("").css_classes(["error"]).halign(gtk::Align::Start).build();
                error_label.set_visible(false);
                let add_button = gtk::Button::builder().label("Add").build();
                add_box.append(&entry_field);
                add_box.append(&add_button);
                let add_row = adw::ActionRow::new();
                let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
                column.append(&add_box);
                column.append(&error_label);
                add_row.set_child(Some(&column));
                group.add(&add_row);
                entry_field.connect_changed({
                    let error_label = error_label.clone();
                    move |_| {
                        error_label.set_visible(false);
                    }
                });
                let add = {
                    let state = state.clone();
                    let account = account.clone();
                    let entry_field = entry_field.clone();
                    let error_label = error_label.clone();
                    let rebuild_handle = rebuild_handle.clone();
                    move || {
                        let Some(entry) = lookout_core::normalize_trust_entry(&entry_field.text()) else {
                            error_label.set_label("That doesn't look like an address or domain - try name@example.com or @example.com.");
                            error_label.set_visible(true);
                            return;
                        };
                        let mut st = state.borrow_mut();
                        st.blocked_senders.insert((account.clone(), entry.clone()));
                        // A sender is never both safe and blocked: drop any
                        // matching safe entry as the block lands.
                        let conflicting: Vec<String> = st
                            .trusted_senders
                            .keys()
                            .filter(|(trusted_account, trusted_entry)| trusted_account == &account && lookout_core::sender_matches_trust_entry(&entry, trusted_entry))
                            .map(|(_, trusted_entry)| trusted_entry.clone())
                            .collect();
                        for trusted_entry in conflicting {
                            st.trusted_senders.remove(&(account.clone(), trusted_entry.clone()));
                            if let Some(db) = &st.ui_db {
                                let _ = db.remove_trusted_sender(&account, &trusted_entry);
                            }
                        }
                        if let Some(db) = &st.ui_db {
                            let _ = db.set_blocked_sender(&account, &entry);
                        }
                        drop(st);
                        entry_field.set_text("");
                        (rebuild_handle.borrow())();
                    }
                };
                let add_for_activate = add.clone();
                add_button.connect_clicked(move |_| add());
                entry_field.connect_activate(move |_| add_for_activate());
                page.add(&group);
            }
        });
    }
    (rebuild.borrow())();

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    root.append(&hint);
    root.append(&page);
    let close_button = gtk::Button::builder().label("Done").css_classes(["suggested-action"]).margin_top(12).build();
    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| dialog.close());
    }
    root.append(&close_button);
    dialog.set_child(Some(&root));
    dialog.present();
}
