//! The "Ignored conversations…" dialog (Config → Mail, item 2.8 / M-20):
//! list and un-ignore the conversations whose arriving mail is moved to
//! Deleted Items. Reads the labels from the UI-state database and writes
//! removals through to it and the in-memory `UiState::ignored_threads` mirror,
//! so the client-rule arrival pass (`rules::enforce_ignored_threads`) picks the
//! change up immediately.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::AccountId;

use crate::util::account_label;
use crate::window::UiState;

/// Presents the modal ignored-conversations window, anchored to `anchor`'s
/// window when one exists (tests have no window).
pub fn show_manage_dialog(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>) {
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Ignored conversations").default_width(560).default_height(480);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);
    let hint = gtk::Label::builder()
        .label("New mail in an ignored conversation goes straight to Deleted Items.")
        .wrap(true)
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();

    // `rebuild` re-renders the per-account sections; each Remove calls it again
    // through the `Rc<RefCell<Box<dyn Fn()>>>` cycle bounded to the dialog.
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
            let (ignored, accounts) = {
                let st = state.borrow();
                let ignored: Vec<(AccountId, String, String)> = st.ui_db.as_ref().and_then(|db| db.load_ignored_threads().ok()).unwrap_or_default();
                let mut accounts: Vec<(AccountId, String)> = st
                    .accounts
                    .iter()
                    .map(|(account, handle)| (account.clone(), account_label(account, &handle.display_name, &handle.email)))
                    .collect();
                accounts.sort_by_key(|a| a.1.to_lowercase());
                (ignored, accounts)
            };
            let group = adw::PreferencesGroup::new();
            if ignored.is_empty() {
                let empty = gtk::Label::builder()
                    .label("No conversations are ignored.")
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
            for (account, thread, label) in ignored {
                let account_label = accounts
                    .iter()
                    .find(|(id, _)| *id == account)
                    .map(|(_, label)| label.clone())
                    .unwrap_or_else(|| account.0.clone());
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&label))
                    .subtitle(glib::markup_escape_text(&account_label))
                    .build();
                let remove = gtk::Button::from_icon_name("user-trash-symbolic");
                remove.set_tooltip_text(Some("Stop ignoring this conversation"));
                remove.add_css_class("flat");
                row.add_suffix(&remove);
                group.add(&row);
                remove.connect_clicked({
                    let state = state.clone();
                    let account = account.clone();
                    let thread = thread.clone();
                    let rebuild_handle = rebuild_handle.clone();
                    move |_| {
                        {
                            let mut st = state.borrow_mut();
                            st.ignored_threads.remove(&(account.clone(), thread.clone()));
                            if let Some(db) = &st.ui_db {
                                let _ = db.remove_ignored_thread(&account, &thread);
                            }
                        }
                        (rebuild_handle.borrow())();
                    }
                });
            }
            page.add(&group);
            groups.borrow_mut().push(group);
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
