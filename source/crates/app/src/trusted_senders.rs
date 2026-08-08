//! The "Trusted senders…" dialog (Config → Mail): add/remove the sender
//! entries that may load remote content, and change their trust level.
//! Everything writes through to the UI-state database (`ui_state_db`) and
//! the in-memory `UiState::trusted_senders` mirror on every change, so the
//! reading pane's load policy picks the change up immediately - the same
//! write-through pattern as the Manage-identities dialog. Entries are
//! keyed per receiving account (a sender trusted on one account is not
//! trusted on another), so each account gets its own section and add row.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{AccountId, TrustLevel};

use crate::window::UiState;

/// A two-option level picker (Images / All content), preselected from
/// `level`. `on_change` fires with the new level when the user picks one.
fn level_dropdown(level: TrustLevel, on_change: impl Fn(TrustLevel) + 'static) -> gtk::DropDown {
    let model = gtk::StringList::new(&[TrustLevel::Images.label(), TrustLevel::AllContent.label()]);
    let dropdown = gtk::DropDown::builder()
        .model(&model)
        .selected(if level == TrustLevel::AllContent { 1 } else { 0 })
        .valign(gtk::Align::Center)
        .build();
    dropdown.connect_selected_notify(move |dropdown| {
        let picked = if dropdown.selected() == 1 { TrustLevel::AllContent } else { TrustLevel::Images };
        on_change(picked);
    });
    dropdown
}

/// The level a two-option [`level_dropdown`] currently shows.
fn dropdown_level(dropdown: &gtk::DropDown) -> TrustLevel {
    if dropdown.selected() == 1 {
        TrustLevel::AllContent
    } else {
        TrustLevel::Images
    }
}

/// A short account label for the section headers: the display name, the
/// account's own address, or the bare account id - whichever is non-empty.
fn account_label(account: &AccountId, display_name: &str, email: &str) -> String {
    if !display_name.trim().is_empty() {
        display_name.to_string()
    } else if !email.trim().is_empty() {
        email.to_string()
    } else {
        account.0.clone()
    }
}

/// Presents the modal trusted-senders window, anchored to `anchor`'s window
/// when one exists (tests have no window).
pub fn show_manage_dialog(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>) {
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Trusted senders").default_width(560).default_height(480);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);
    let hint = gtk::Label::builder()
        .label("Trusted senders may load remote images - or, at the higher level, every remote style, font and other content - that Lookout blocks by default.")
        .wrap(true)
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();

    // `rebuild` re-renders the per-account sections; its per-row handlers
    // call it again after an add/remove, and reach it through the
    // `Rc<RefCell<Box<dyn Fn()>>>` (which forms a reference cycle that
    // lives until the dialog's widgets drop - a bounded, one-off cost for a
    // modal dialog, as in the identities dialog).
    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    let rebuild_handle = rebuild.clone();
    {
        let state = state.clone();
        let page = page.clone();
        let rebuild_handle = rebuild_handle.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            while let Some(child) = page.first_child() {
                child.unparent();
            }
            let (entries, accounts) = {
                let st = state.borrow();
                let entries: Vec<(AccountId, String, TrustLevel)> = st
                    .trusted_senders
                    .iter()
                    .map(|((account, entry), level)| (account.clone(), entry.clone(), *level))
                    .collect();
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
                    .label("No mail accounts connected yet - trusted senders are remembered per account.")
                    .wrap(true)
                    .halign(gtk::Align::Start)
                    .css_classes(["dim-label"])
                    .margin_top(12)
                    .margin_bottom(12)
                    .build();
                group.add(&empty);
                page.add(&group);
                return;
            }
            for (account, label) in accounts {
                let group = adw::PreferencesGroup::builder().title(glib::markup_escape_text(&label)).build();
                let mut account_entries: Vec<(String, TrustLevel)> = entries.iter().filter(|(a, _, _)| *a == account).map(|(_, entry, level)| (entry.clone(), *level)).collect();
                account_entries.sort_by(|a, b| a.0.cmp(&b.0));
                for (entry, level) in &account_entries {
                    let row = adw::ActionRow::builder().title(glib::markup_escape_text(entry)).build();
                    let remove = gtk::Button::from_icon_name("user-trash-symbolic");
                    remove.set_tooltip_text(Some("Remove this sender"));
                    remove.add_css_class("flat");
                    let suffix = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
                    let dropdown = level_dropdown(*level, {
                        let state = state.clone();
                        let account = account.clone();
                        let entry = entry.clone();
                        move |picked| {
                            state.borrow_mut().trusted_senders.insert((account.clone(), entry.clone()), picked);
                            if let Some(db) = &state.borrow().ui_db {
                                let _ = db.set_trusted_sender(&account, &entry, picked);
                            }
                        }
                    });
                    suffix.append(&dropdown);
                    suffix.append(&remove);
                    row.add_suffix(&suffix);
                    group.add(&row);
                    remove.connect_clicked({
                        let state = state.clone();
                        let account = account.clone();
                        let entry = entry.clone();
                        let rebuild_handle = rebuild_handle.clone();
                        move |_| {
                            state.borrow_mut().trusted_senders.remove(&(account.clone(), entry.clone()));
                            if let Some(db) = &state.borrow().ui_db {
                                let _ = db.remove_trusted_sender(&account, &entry);
                            }
                            (rebuild_handle.borrow())();
                        }
                    });
                }
                // Per-account add row: entry field + level picker + Add.
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
                let add_dropdown = level_dropdown(TrustLevel::Images, |_| {});
                add_box.append(&entry_field);
                add_box.append(&add_dropdown);
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
                    let add_dropdown = add_dropdown.clone();
                    let error_label = error_label.clone();
                    let rebuild_handle = rebuild_handle.clone();
                    move || {
                        let Some(entry) = lookout_core::normalize_trust_entry(&entry_field.text()) else {
                            error_label.set_label("That doesn't look like an address or domain - try name@example.com or @example.com.");
                            error_label.set_visible(true);
                            return;
                        };
                        let level = dropdown_level(&add_dropdown);
                        state.borrow_mut().trusted_senders.insert((account.clone(), entry.clone()), level);
                        if let Some(db) = &state.borrow().ui_db {
                            let _ = db.set_trusted_sender(&account, &entry, level);
                        }
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
