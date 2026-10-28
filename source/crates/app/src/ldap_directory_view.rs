//! Settings → People's LDAP directory dialog (OUTLOOK_PARITY.md item 4.7.9).
//!
//! The Add/edit form is modelled on `other_accounts::show_add_account_dialog`:
//! the structured entry (host, base DN, attributes, enabled) goes to
//! `settings.json` through [`AppConfig`], while the bind password goes to the
//! keyring's `PROTOCOL_LDAP` slot keyed by the directory id. A blank password
//! on edit means "keep the stored secret"; anonymous binds need none.
//!
//! Validation lives in [`ldap_directory_problems`] and [`build_ldap_directory`]
//! so it is unit-tested without a dialog or a live server.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::AccountId;

use crate::app_config::{AppConfig, LdapDirectory};
use crate::other_accounts::{field_label, port_label, KeyringStore, PROTOCOL_LDAP};
use crate::worker::Worker;

/// The dialog's plain values, separated from the widgets so validation and the
/// `LdapDirectory` mapping are pure. `bind_password` is transient: it is
/// written to the keyring, never to `settings.json`.
#[derive(Clone, Debug)]
pub(crate) struct LdapDirectoryForm {
    pub account_id: AccountId,
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub bind_dn: String,
    pub bind_password: String,
    pub base_dn: String,
    pub filter: String,
    pub mail_attr: String,
    pub name_attr: String,
    pub enabled: bool,
}

/// What is wrong with the form, as the dialog lists it. `existing` supplies the
/// edit-time rules: a blank password is allowed when the stored entry already
/// had a bound (non-anonymous) bind DN, because blank means "keep the secret".
pub(crate) fn ldap_directory_problems(existing: Option<&LdapDirectory>, form: &LdapDirectoryForm) -> Vec<String> {
    let mut problems = Vec::new();
    if form.account_id.0.trim().is_empty() {
        problems.push("a mail account is required".to_string());
    }
    if form.display_name.trim().is_empty() {
        problems.push("a name is required".to_string());
    }
    if form.host.trim().is_empty() {
        problems.push("a host is required".to_string());
    }
    if form.base_dn.trim().is_empty() {
        problems.push("a search base DN is required".to_string());
    }
    if form.port == 0 {
        problems.push("a port is required".to_string());
    }
    let bound_before = existing.is_some_and(|directory| !directory.bind_dn.trim().is_empty());
    if !form.bind_dn.trim().is_empty() && form.bind_password.is_empty() && !bound_before {
        problems.push("a bind password is required for a non-anonymous bind".to_string());
    }
    problems
}

/// Maps a validated form to the persisted entry. The id is preserved on edit
/// and freshly generated (`ldap:<uuid>`) on create; empty optional fields stay
/// empty, which the LDAP backend reads as its `mail`/`displayName` defaults.
pub(crate) fn build_ldap_directory(existing: Option<&LdapDirectory>, form: LdapDirectoryForm) -> LdapDirectory {
    LdapDirectory {
        id: existing.map(|directory| directory.id.clone()).unwrap_or_else(AppConfig::new_ldap_directory_id),
        account_id: form.account_id,
        display_name: form.display_name.trim().to_string(),
        host: form.host.trim().to_string(),
        port: form.port,
        tls: form.tls,
        bind_dn: form.bind_dn.trim().to_string(),
        base_dn: form.base_dn.trim().to_string(),
        filter: form.filter.trim().to_string(),
        mail_attr: form.mail_attr.trim().to_string(),
        name_attr: form.name_attr.trim().to_string(),
        enabled: form.enabled,
    }
}

/// Opens the add/edit LDAP directory dialog, anchored to `anchor`. `accounts`
/// is the connected mail accounts (id plus a display label) the directory can
/// be scoped to. Adding needs at least one (the caller checks); editing keeps
/// the entry's own account selectable even when it is disconnected. `on_saved`
/// fires on the UI thread once the entry and (when changed) the keyring secret
/// are stored, so the caller can repaint Settings and the People Directory
/// row.
pub(crate) fn show_ldap_directory_dialog(
    anchor: &gtk::Widget,
    worker: Rc<Worker>,
    app_config: Rc<RefCell<AppConfig>>,
    keyring: Arc<dyn KeyringStore>,
    accounts: &[(AccountId, String)],
    existing: Option<LdapDirectory>,
    on_saved: Rc<dyn Fn()>,
) -> gtk::Window {
    let is_edit = existing.is_some();
    let dialog = {
        let mut builder = gtk::Window::builder()
            .modal(true)
            .title(if is_edit { "Edit LDAP directory" } else { "Add LDAP directory" })
            .default_width(620)
            .default_height(660);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    // -- fields --
    let name_entry = gtk::Entry::builder().placeholder_text("Company directory").build();
    let host_entry = gtk::Entry::builder().placeholder_text("ldap.example.com").build();
    let port_spin = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    port_spin.set_value(389.0);
    let tls_switch = gtk::Switch::new();
    let bind_dn_entry = gtk::Entry::builder()
        .placeholder_text("cn=Directory Reader,dc=example,dc=com (blank for anonymous)")
        .build();
    let bind_password_entry = gtk::PasswordEntry::builder()
        .placeholder_text(if is_edit {
            "Leave blank to keep the saved password"
        } else {
            "Bind password (blank for anonymous)"
        })
        .build();
    let base_dn_entry = gtk::Entry::builder().placeholder_text("dc=example,dc=com").build();
    let filter_entry = gtk::Entry::builder().placeholder_text("Optional extra filter, e.g. (objectClass=person)").build();
    let mail_attr_entry = gtk::Entry::builder().placeholder_text("mail").build();
    let name_attr_entry = gtk::Entry::builder().placeholder_text("displayName").build();
    let enabled_switch = gtk::Switch::new();

    // -- account picker --
    // The entry's own account stays selectable even when it is no longer
    // connected, so editing never silently re-scopes a directory.
    let mut account_ids: Vec<AccountId> = accounts.iter().map(|(account_id, _)| account_id.clone()).collect();
    let mut account_labels: Vec<String> = accounts.iter().map(|(_, label)| label.clone()).collect();
    let selected_account = existing.as_ref().map(|directory| directory.account_id.clone()).or_else(|| account_ids.first().cloned());
    if let Some(selected) = &selected_account {
        if !account_ids.contains(selected) {
            account_ids.push(selected.clone());
            account_labels.push(format!("{} (not connected)", selected.0));
        }
    }
    let label_refs: Vec<&str> = account_labels.iter().map(String::as_str).collect();
    let account_dropdown = gtk::DropDown::from_strings(&label_refs);
    // The account labels carry addresses: content for privacy mode.
    crate::privacy::mark_content(&account_dropdown);
    if let Some(selected) = &selected_account {
        if let Some(index) = account_ids.iter().position(|account_id| account_id == selected) {
            account_dropdown.set_selected(index as u32);
        }
    }

    // -- seed from an existing entry --
    if let Some(directory) = &existing {
        name_entry.set_text(&directory.display_name);
        host_entry.set_text(&directory.host);
        port_spin.set_value(directory.port as f64);
        tls_switch.set_active(directory.tls);
        bind_dn_entry.set_text(&directory.bind_dn);
        base_dn_entry.set_text(&directory.base_dn);
        filter_entry.set_text(&directory.filter);
        mail_attr_entry.set_text(&directory.mail_attr);
        name_attr_entry.set_text(&directory.name_attr);
        enabled_switch.set_active(directory.enabled);
    } else {
        // A directory the user explicitly adds is on unless they turn it off.
        enabled_switch.set_active(true);
    }

    // -- form grid --
    let form = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(8)
        .halign(gtk::Align::Fill)
        .hexpand(true)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let mut row = 0;
    form.attach(&field_label("Name"), 0, row, 1, 1);
    form.attach(&name_entry, 1, row, 3, 1);
    row += 1;
    form.attach(&field_label("Use for"), 0, row, 1, 1);
    form.attach(&account_dropdown, 1, row, 3, 1);
    row += 1;
    form.attach(&field_label("Server"), 0, row, 1, 1);
    form.attach(&host_entry, 1, row, 1, 1);
    form.attach(&port_label("Port"), 2, row, 1, 1);
    form.attach(&port_spin, 3, row, 1, 1);
    row += 1;
    let tls_note = gtk::Label::builder()
        .label("Encryption uses implicit TLS on port 636 or 3269 and StartTLS on any other port.")
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    crate::privacy::mark_ui(&tls_note);
    form.attach(&tls_note, 1, row, 3, 1);
    row += 1;
    form.attach(&field_label("Encryption"), 0, row, 1, 1);
    form.attach(&labeled_switch("Use TLS", &tls_switch), 1, row, 3, 1);
    row += 1;
    form.attach(&field_label("Bind DN"), 0, row, 1, 1);
    form.attach(&bind_dn_entry, 1, row, 3, 1);
    row += 1;
    form.attach(&field_label("Password"), 0, row, 1, 1);
    form.attach(&bind_password_entry, 1, row, 3, 1);
    row += 1;
    form.attach(&field_label("Search base"), 0, row, 1, 1);
    form.attach(&base_dn_entry, 1, row, 3, 1);
    row += 1;
    form.attach(&field_label("Filter"), 0, row, 1, 1);
    form.attach(&filter_entry, 1, row, 3, 1);
    row += 1;
    form.attach(&field_label("Mail attribute"), 0, row, 1, 1);
    form.attach(&mail_attr_entry, 1, row, 1, 1);
    form.attach(&field_label("Name attribute"), 2, row, 1, 1);
    form.attach(&name_attr_entry, 3, row, 1, 1);
    row += 1;
    form.attach(&field_label("Enabled"), 0, row, 1, 1);
    form.attach(&labeled_switch("Search this directory", &enabled_switch), 1, row, 3, 1);
    row += 1;

    let error_label = gtk::Label::builder().halign(gtk::Align::Start).css_classes(["error"]).wrap(true).visible(false).build();
    form.attach(&error_label, 0, row, 4, 1);

    // -- footer --
    let footer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let cancel_button = gtk::Button::with_label("Cancel");
    let save_button = gtk::Button::with_label(if is_edit { "Save" } else { "Add directory" });
    save_button.add_css_class("suggested-action");
    footer.append(&cancel_button);
    footer.append(&save_button);

    let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let scroller = gtk::ScrolledWindow::builder().child(&form).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    root.append(&scroller);
    root.append(&footer);
    dialog.set_child(Some(&root));

    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }

    let save = {
        let existing = existing.clone();
        let account_ids = account_ids.clone();
        let worker = worker.clone();
        let keyring = keyring.clone();
        let app_config = app_config.clone();
        let dialog = dialog.clone();
        let on_saved = on_saved.clone();
        let name_entry = name_entry.clone();
        let host_entry = host_entry.clone();
        let port_spin = port_spin.clone();
        let tls_switch = tls_switch.clone();
        let bind_dn_entry = bind_dn_entry.clone();
        let bind_password_entry = bind_password_entry.clone();
        let base_dn_entry = base_dn_entry.clone();
        let filter_entry = filter_entry.clone();
        let mail_attr_entry = mail_attr_entry.clone();
        let name_attr_entry = name_attr_entry.clone();
        let enabled_switch = enabled_switch.clone();
        let account_dropdown = account_dropdown.clone();
        let error_label = error_label.clone();
        let save_button = save_button.clone();
        move || {
            let form = LdapDirectoryForm {
                account_id: account_ids.get(account_dropdown.selected() as usize).cloned().unwrap_or_else(|| AccountId(String::new())),
                display_name: name_entry.text().to_string(),
                host: host_entry.text().to_string(),
                port: port_spin.value_as_int().clamp(1, 65535) as u16,
                tls: tls_switch.is_active(),
                bind_dn: bind_dn_entry.text().to_string(),
                bind_password: bind_password_entry.text().to_string(),
                base_dn: base_dn_entry.text().to_string(),
                filter: filter_entry.text().to_string(),
                mail_attr: mail_attr_entry.text().to_string(),
                name_attr: name_attr_entry.text().to_string(),
                enabled: enabled_switch.is_active(),
            };
            let problems = ldap_directory_problems(existing.as_ref(), &form);
            if !problems.is_empty() {
                error_label.set_label(&problems.join("\n"));
                error_label.set_visible(true);
                return;
            }
            error_label.set_visible(false);

            let password = form.bind_password.clone();
            let directory = build_ldap_directory(existing.as_ref(), form);
            let directory_id = directory.id.clone();

            // Keyring writes happen on the worker thread (D-Bus I/O); the
            // config mutation + save only run once the secret is stored, so a
            // failed write leaves settings.json untouched.
            save_button.set_sensitive(false);
            let keyring = keyring.clone();
            let app_config = app_config.clone();
            let dialog = dialog.clone();
            let on_saved = on_saved.clone();
            let save_button = save_button.clone();
            let error_label = error_label.clone();
            let (result_tx, result_rx) = async_channel::bounded(1);
            worker.spawn(async move {
                let result = if password.is_empty() {
                    Ok(())
                } else {
                    keyring.store_password(&AccountId(directory_id), PROTOCOL_LDAP, &password).await
                };
                let _ = result_tx.send(result).await;
            });
            glib::spawn_future_local(async move {
                let Ok(result) = result_rx.recv().await else { return };
                match result {
                    Ok(()) => {
                        app_config.borrow_mut().upsert_ldap_directory(directory.clone());
                        crate::app_config::save(&app_config.borrow());
                        dialog.close();
                        on_saved();
                    }
                    Err(e) => {
                        save_button.set_sensitive(true);
                        error_label.set_label(&e);
                        error_label.set_visible(true);
                    }
                }
            });
        }
    };
    let save_for_activate = {
        let save = save.clone();
        move || save()
    };
    save_button.connect_clicked(move |_| save());
    name_entry.connect_activate(move |_| save_for_activate());

    dialog.present();
    dialog
}

/// A form row with a caption and a switch on the right.
fn labeled_switch(label: &str, switch: &gtk::Switch) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).hexpand(true).build();
    let text = gtk::Label::builder().label(label).hexpand(true).halign(gtk::Align::Start).build();
    crate::privacy::mark_ui(&text);
    row.append(&text);
    row.append(switch);
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> LdapDirectoryForm {
        LdapDirectoryForm {
            account_id: AccountId("account_1".into()),
            display_name: "Company directory".into(),
            host: "ldap.example.com".into(),
            port: 389,
            tls: false,
            bind_dn: String::new(),
            bind_password: String::new(),
            base_dn: "dc=example,dc=com".into(),
            filter: String::new(),
            mail_attr: String::new(),
            name_attr: String::new(),
            enabled: true,
        }
    }

    fn existing() -> LdapDirectory {
        LdapDirectory {
            id: "ldap:existing".into(),
            account_id: AccountId("account_1".into()),
            display_name: "Old name".into(),
            host: "ldap.example.com".into(),
            port: 389,
            tls: false,
            bind_dn: "cn=reader,dc=example,dc=com".into(),
            base_dn: "dc=example,dc=com".into(),
            filter: String::new(),
            mail_attr: String::new(),
            name_attr: String::new(),
            enabled: true,
        }
    }

    #[test]
    fn validation_requires_a_name_host_base_dn_and_account() {
        let mut invalid = form();
        invalid.account_id = AccountId(String::new());
        invalid.display_name = "  ".into();
        invalid.host = String::new();
        invalid.base_dn = "\t".into();
        let problems = ldap_directory_problems(None, &invalid);
        assert!(problems.contains(&"a mail account is required".to_string()));
        assert!(problems.contains(&"a name is required".to_string()));
        assert!(problems.contains(&"a host is required".to_string()));
        assert!(problems.contains(&"a search base DN is required".to_string()));
    }

    #[test]
    fn a_bound_directory_needs_a_password_unless_one_is_already_stored() {
        let mut bound = form();
        bound.bind_dn = "cn=reader,dc=example,dc=com".into();
        // A new bound entry needs the secret up front.
        assert!(ldap_directory_problems(None, &bound).iter().any(|problem| problem.contains("bind password")));
        // An edit with a blank password keeps the stored one.
        assert!(ldap_directory_problems(Some(&existing()), &bound).is_empty());
        // An anonymous bind needs no password at all.
        assert!(ldap_directory_problems(None, &form()).is_empty());
    }

    #[test]
    fn build_preserves_the_id_on_edit_and_generates_one_on_create() {
        let mut changed = form();
        changed.display_name = "  Renamed  ".into();
        changed.host = " ldap.example.org ".into();
        let edited = build_ldap_directory(Some(&existing()), changed.clone());
        assert_eq!(edited.id, "ldap:existing");
        assert_eq!(edited.display_name, "Renamed");
        assert_eq!(edited.host, "ldap.example.org");

        let created = build_ldap_directory(None, changed);
        assert!(created.id.starts_with(crate::app_config::LDAP_DIRECTORY_ID_PREFIX));
        assert_ne!(created.id, "ldap:existing");
        // Empty optional fields stay empty, so the backend applies its
        // `mail`/`displayName` defaults.
        assert!(created.filter.is_empty());
        assert!(created.mail_attr.is_empty());
        assert!(created.name_attr.is_empty());
        assert!(created.enabled);
    }
}
