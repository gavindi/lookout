//! Calendar group and rename dialogs (OUTLOOK_PARITY.md item 3.13 / CAL-33).
//!
//! The Calendar sidebar's checklist (see `calendar_view::rebuild_calendar_checklist`)
//! names the intent - "New group", "Rename group", "Rename calendar" - and the
//! window's action handler routes it here for the name prompt. Group membership
//! and the hidden-calendar list live in `settings.json`
//! (`AppConfig::calendar_groups` / `hidden_calendars`); the handler persists
//! them. A single-entry dialog is all these need, so one helper covers all
//! three, mirroring the Tasks list dialogs' shape (`task_lists.rs`).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use adw::prelude::*;

/// Shows a modal single-entry dialog and calls `on_accept` with the trimmed,
/// non-empty name. `title` is the dialog's window title, `entry_title` the
/// entry row's label, `accept_label` the confirm button's text, and `initial`
/// the prefilled value (empty for a create). The dialog closes itself once the
/// name is accepted.
pub(crate) fn show_name_dialog(window: &adw::ApplicationWindow, title: &str, entry_title: &str, accept_label: &str, initial: &str, on_accept: impl Fn(String) + 'static) {
    let dialog = adw::Window::builder().transient_for(window).modal(true).title(title).default_width(420).build();
    let name_entry = adw::EntryRow::builder().title(entry_title).build();
    if !initial.is_empty() {
        name_entry.set_text(initial);
    }
    let group = adw::PreferencesGroup::new();
    group.add(&name_entry);

    let cancel = gtk::Button::with_label("Cancel");
    let accept = gtk::Button::with_label(accept_label);
    accept.add_css_class("suggested-action");
    let buttons = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).halign(gtk::Align::End).build();
    buttons.append(&cancel);
    buttons.append(&accept);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&group);
    content.append(&buttons);
    dialog.set_content(Some(&content));

    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    {
        let dialog = dialog.clone();
        let on_accept = std::rc::Rc::new(on_accept);
        // Enter in the entry row accepts, as in the Tasks list dialogs.
        let accept_clicked = {
            let on_accept = on_accept.clone();
            let name_entry = name_entry.clone();
            let dialog = dialog.clone();
            move || {
                let name = name_entry.text().trim().to_string();
                if name.is_empty() {
                    return;
                }
                on_accept(name);
                dialog.close();
            }
        };
        {
            let accept_clicked = accept_clicked.clone();
            accept.connect_clicked(move |_| accept_clicked());
        }
        name_entry.connect_entry_activated(move |_| accept_clicked());
    }
    dialog.present();
}
