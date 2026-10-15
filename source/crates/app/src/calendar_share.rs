//! The "Share calendar" dialog for a Microsoft 365 calendar
//! (OUTLOOK_PARITY.md item 5.6 / CAL-24).
//!
//! Lists the people the calendar is currently shared with (and at what role),
//! and lets the user add a person at one of Outlook's four roles, change or
//! revoke a grant. Backed by Graph `calendarPermissions` through
//! [`GraphCalendarShareClient`]. Only reachable for a calendar whose account is
//! a consented Microsoft 365 account; the caller gates it - a CalDAV calendar
//! has no equivalent in this item.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use lookout_core::CalendarInfo;

use crate::graph_calendar_share::{role_for_index, role_label, GraphCalendarShareClient, SHARE_ROLES};
use crate::util::clear_box;

/// The reload closure slot: the permission list's rebuild and the row Remove
/// handlers are mutually referential, so the rebuild is stashed here and
/// resolved at call time (the `ManageRebuild` idiom in `window/imip.rs`).
type ReloadSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// Shows the dialog for one calendar the user owns. A no-op visible only in the
/// consent-needed state when the account hasn't granted Graph access yet.
pub(crate) fn show_share_calendar_dialog(window: &adw::ApplicationWindow, calendar: CalendarInfo, toast_overlay: &adw::ToastOverlay) {
    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title("Share calendar")
        .default_width(460)
        .default_height(520)
        .build();

    let client = Rc::new(GraphCalendarShareClient::new(calendar.account_id.clone()));

    let heading = gtk::Label::builder().label(calendar.display_name.clone()).xalign(0.0).css_classes(["title-4"]).build();

    // --- The people the calendar is already shared with. Rebuilt on every
    // load/change; each row carries its grant id and role.
    let people_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let people_group = adw::PreferencesGroup::builder().title("Shared with").build();
    people_group.add(&people_box);

    // --- Add a person: address + role + optional invitation email.
    let email_entry = adw::EntryRow::builder().title("Name or email address").build();
    let role_dropdown = gtk::DropDown::builder().build();
    let role_labels: Vec<&str> = SHARE_ROLES.iter().map(|(_, label)| *label).collect();
    role_dropdown.set_model(Some(&gtk::StringList::new(&role_labels)));
    role_dropdown.set_selected(0);
    role_dropdown.set_tooltip_text(Some("Permission level"));
    let notify_row = adw::SwitchRow::builder().title("Send an invitation").active(true).build();
    let share_button = gtk::Button::with_label("Share");
    share_button.add_css_class("suggested-action");
    share_button.set_halign(gtk::Align::End);
    let add_group = adw::PreferencesGroup::builder().title("Add people").build();
    add_group.add(&email_entry);
    add_group.add(&role_dropdown);
    add_group.add(&notify_row);
    add_group.add(&share_button);

    // The reload closure rebuilds the people list from the server. It is a slot
    // because the row Remove handlers call it and it creates those handlers.
    let reload: ReloadSlot = Rc::new(RefCell::new(None));
    *reload.borrow_mut() = Some(Rc::new({
        let people_box = people_box.clone();
        let client = client.clone();
        let calendar = calendar.clone();
        let toast_overlay = toast_overlay.clone();
        let reload = reload.clone();
        move || {
            let people_box = people_box.clone();
            let client = client.clone();
            let calendar = calendar.clone();
            let toast_overlay = toast_overlay.clone();
            let reload = reload.clone();
            glib::spawn_future_local(async move {
                let permissions = match client.list_permissions(&calendar).await {
                    Ok(permissions) => permissions,
                    Err(e) => {
                        toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't load sharing: {e}"))));
                        return;
                    }
                };
                clear_box(&people_box);
                if permissions.is_empty() {
                    people_box.append(
                        &gtk::Label::builder()
                            .label("This calendar isn't shared with anyone yet")
                            .xalign(0.0)
                            .css_classes(["dim-label", "caption"])
                            .build(),
                    );
                }
                for permission in permissions {
                    people_box.append(&permission_row(&permission, &calendar, &client, &toast_overlay, reload.clone()));
                }
            });
        }
    }));

    share_button.connect_clicked({
        let email_entry = email_entry.clone();
        let role_dropdown = role_dropdown.clone();
        let notify_row = notify_row.clone();
        let client = client.clone();
        let calendar = calendar.clone();
        let toast_overlay = toast_overlay.clone();
        let reload = reload.clone();
        move |_| {
            let address = email_entry.text().trim().to_string();
            if address.is_empty() {
                toast_overlay.add_toast(adw::Toast::new("Enter a name or email address to share with."));
                return;
            }
            let role = role_for_index(role_dropdown.selected());
            let send_notification = notify_row.is_active();
            let client = client.clone();
            let calendar = calendar.clone();
            let toast_overlay = toast_overlay.clone();
            let reload = reload.clone();
            let email_entry = email_entry.clone();
            glib::spawn_future_local(async move {
                match client.create_permission(&calendar, &address, role, send_notification).await {
                    Ok(_) => {
                        email_entry.set_text("");
                        toast_overlay.add_toast(adw::Toast::new(&format!("Shared with {address}")));
                        if let Some(reload) = reload.borrow().clone() {
                            reload();
                        }
                    }
                    Err(e) => toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't share: {e}")))),
                }
            });
        }
    });

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    content.append(&heading);
    if client.consented() {
        content.append(&people_group);
        content.append(&add_group);
    } else {
        let setup = adw::PreferencesGroup::builder().title("Sharing needs Outlook access").build();
        let button = gtk::Button::with_label("Allow sharing");
        button.add_css_class("suggested-action");
        button.set_halign(gtk::Align::Start);
        let client = client.clone();
        let dialog = dialog.clone();
        let toast_overlay = toast_overlay.clone();
        button.connect_clicked(move |_| {
            let client = client.clone();
            let dialog = dialog.clone();
            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                match client.ensure_consent().await {
                    Ok(()) => {
                        dialog.close();
                        toast_overlay.add_toast(adw::Toast::new("Outlook sharing enabled - reopen Share to manage it."));
                    }
                    Err(e) => toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&e))),
                }
            });
        });
        setup.add(&button);
        content.append(&setup);
    }

    let scroller = gtk::ScrolledWindow::builder()
        .child(&content)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    dialog.set_content(Some(&scroller));
    dialog.present();

    if client.consented() {
        if let Some(reload) = reload.borrow().clone() {
            reload();
        }
    }
}

/// One existing grant's row: name/address, the role, and a Revoke button.
fn permission_row(
    permission: &lookout_graph::CalendarPermission,
    calendar: &CalendarInfo,
    client: &Rc<GraphCalendarShareClient>,
    toast_overlay: &adw::ToastOverlay,
    reload: ReloadSlot,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let text = gtk::Label::builder()
        .label(permission.email.clone().unwrap_or_else(|| permission.name.clone().unwrap_or_else(|| "Unknown".to_string())))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let role = gtk::Label::builder()
        .label(role_label(permission.role))
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build();
    let remove = gtk::Button::from_icon_name("user-trash-symbolic");
    remove.add_css_class("flat");
    remove.set_tooltip_text(Some("Remove"));
    if let Some(permission_id) = permission.id.clone() {
        let client = client.clone();
        let calendar = calendar.clone();
        let toast_overlay = toast_overlay.clone();
        remove.connect_clicked(move |_| {
            let client = client.clone();
            let calendar = calendar.clone();
            let toast_overlay = toast_overlay.clone();
            let reload = reload.clone();
            let permission_id = permission_id.clone();
            glib::spawn_future_local(async move {
                match client.delete_permission(&calendar, &permission_id).await {
                    Ok(()) => {
                        toast_overlay.add_toast(adw::Toast::new("Sharing removed"));
                        if let Some(reload) = reload.borrow().clone() {
                            reload();
                        }
                    }
                    Err(e) => toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't remove: {e}")))),
                }
            });
        });
    } else {
        remove.set_sensitive(false);
    }
    row.append(&text);
    row.append(&role);
    row.append(&remove);
    row
}
