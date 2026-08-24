//! Manual IMAP/SMTP accounts ("other accounts"): mail accounts Lookout
//! manages itself rather than sourcing from GNOME Online Accounts.
//!
//! The account list itself is relational data, so it lives in `settings.json`
//! (`AppConfig::other_accounts`) exactly like sending identities and webcal
//! subscriptions. Passwords never touch that file - they go in the GNOME
//! keyring via the Secret Service D-Bus API (`secret-service` crate, the same
//! zbus stack the GOA integration uses), one item per `(account, protocol)`,
//! addressed by stable attributes so an account's secrets can be found,
//! replaced, and deleted without ever reading them back.
//!
//! [`OtherCredentialProvider`] bridges the keyring to `lookout-mail`'s
//! `CredentialProvider` trait, mirroring `goa_credentials.rs`: a fresh
//! password is fetched on every (re)connect attempt and SMTP send, and never
//! cached by the mail engine.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use async_trait::async_trait;
use gtk::glib;
use lookout_core::AccountId;
use lookout_mail::session::CredentialProvider;
use lookout_mail::Credential;
use secret_service::{EncryptionType, SecretService};

use crate::app_config::{AppConfig, OtherAccount, OTHER_ACCOUNT_ID_PREFIX};
use crate::worker::Worker;

const PROTOCOL_IMAP: &str = "imap";
const PROTOCOL_SMTP: &str = "smtp";

/// The keyring item attribute identifying Lookout's own items, so the
/// passwords show up under a recognizable application name in Seahorse and
/// are never confused with another app's secrets.
const KEYRING_APPLICATION: &str = "io.github.gavindi.Lookout";

// ---------------------------------------------------------------------------
// Keyring access
// ---------------------------------------------------------------------------

/// Where an account's passwords are stored and how they're fetched. Behind a
/// trait so the credential provider and the account dialog are testable
/// against a fake store (no D-Bus/secret service daemon in CI). Implemented
/// by [`SecretServiceKeyring`] in production.
#[async_trait]
pub trait KeyringStore: Send + Sync {
    /// The stored password for `account_id`'s protocol slot (`imap`/`smtp`).
    async fn load_password(&self, account_id: &AccountId, protocol: &str) -> Result<String, String>;
    /// Stores (or replaces) the password for one protocol slot.
    async fn store_password(&self, account_id: &AccountId, protocol: &str, password: &str) -> Result<(), String>;
    /// Removes every stored secret for `account_id` (both protocol slots).
    async fn delete_account(&self, account_id: &AccountId) -> Result<(), String>;
}

/// The production keyring: GNOME Keyring / any Secret Service provider,
/// reached over the session D-Bus. Stateless - each call opens its own
/// connection, matching the "fetch fresh credentials per attempt" philosophy
/// of the GOA bridge. `Send + Sync`, so one `Arc` can be shared between the
/// UI-thread dialog, the worker-thread credential fetches, and removal.
pub struct SecretServiceKeyring;

impl SecretServiceKeyring {
    pub fn new() -> Arc<dyn KeyringStore> {
        Arc::new(SecretServiceKeyring)
    }
}

/// The lookup attributes for one account's protocol slot. Every operation
/// addresses items by these, never by label.
fn slot_attributes<'a>(account_id: &'a AccountId, protocol: &'a str) -> HashMap<&'a str, &'a str> {
    let mut attrs = HashMap::new();
    attrs.insert("application", KEYRING_APPLICATION);
    attrs.insert("account", account_id.0.as_str());
    attrs.insert("protocol", protocol);
    attrs
}

/// All of an account's items (both protocol slots): `application` + `account`
/// match anything the account owns regardless of protocol.
fn account_attributes(account_id: &AccountId) -> HashMap<&str, &str> {
    let mut attrs = HashMap::new();
    attrs.insert("application", KEYRING_APPLICATION);
    attrs.insert("account", account_id.0.as_str());
    attrs
}

/// Turns a `secret-service` error into a user-facing message. The exotic
/// cases (no provider on the bus, no default collection, a dismissed
/// unlock prompt) get explicit explanations because they're the ones the
/// user can actually do something about. Shared with `assistant.rs`, which
/// stores the Assistant API token in the same keyring.
pub(crate) fn keyring_error(e: secret_service::Error) -> String {
    match e {
        secret_service::Error::Unavailable => {
            "No GNOME keyring is available - start the keyring service (e.g. install and run gnome-keyring) to store account passwords.".to_string()
        }
        secret_service::Error::NoResult => "The keyring has no default collection - create one (for example with Seahorse) to store account passwords.".to_string(),
        secret_service::Error::Prompt => "The keyring unlock prompt was dismissed.".to_string(),
        secret_service::Error::Locked => "The keyring is locked - unlock it to access stored passwords.".to_string(),
        other => format!("Keyring error: {other}"),
    }
}

#[async_trait]
impl KeyringStore for SecretServiceKeyring {
    async fn load_password(&self, account_id: &AccountId, protocol: &str) -> Result<String, String> {
        let service = SecretService::connect(EncryptionType::Dh).await.map_err(keyring_error)?;
        let collection = service.get_default_collection().await.map_err(keyring_error)?;
        let items = collection.search_items(slot_attributes(account_id, protocol)).await.map_err(keyring_error)?;
        let Some(item) = items.into_iter().next() else {
            return Err(format!("No saved password for {} ({protocol}).", account_id.0));
        };
        let secret = item.get_secret().await.map_err(keyring_error)?;
        String::from_utf8(secret).map_err(|_| "The stored password is not valid text.".to_string())
    }

    async fn store_password(&self, account_id: &AccountId, protocol: &str, password: &str) -> Result<(), String> {
        let service = SecretService::connect(EncryptionType::Dh).await.map_err(keyring_error)?;
        let collection = service.get_default_collection().await.map_err(keyring_error)?;
        // Best-effort unlock: a locked default collection prompts the user
        // through the keyring daemon before the write lands.
        let _ = collection.unlock().await;
        collection
            .create_item(
                &format!("Lookout · {} · {}", account_id.0, protocol),
                slot_attributes(account_id, protocol),
                password.as_bytes(),
                true,
                "text/plain",
            )
            .await
            .map_err(keyring_error)?;
        Ok(())
    }

    async fn delete_account(&self, account_id: &AccountId) -> Result<(), String> {
        let service = SecretService::connect(EncryptionType::Dh).await.map_err(keyring_error)?;
        let collection = service.get_default_collection().await.map_err(keyring_error)?;
        let items = collection.search_items(account_attributes(account_id)).await.map_err(keyring_error)?;
        for item in items {
            item.delete().await.map_err(keyring_error)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Credential provider
// ---------------------------------------------------------------------------

/// Feeds `lookout-mail` the keyring-stored password for one manual account.
/// Both protocol slots always exist (the save flow writes the same password
/// into both when the user opted for a single password), so IMAP and SMTP
/// each read their own slot - the `use_same_password` flag is purely a
/// dialog convenience and never consulted at connect time.
pub struct OtherCredentialProvider {
    account_id: AccountId,
    keyring: Arc<dyn KeyringStore>,
}

impl OtherCredentialProvider {
    pub fn new(account_id: AccountId, keyring: Arc<dyn KeyringStore>) -> Self {
        OtherCredentialProvider { account_id, keyring }
    }
}

#[async_trait]
impl CredentialProvider for OtherCredentialProvider {
    async fn imap_credential(&self) -> Result<Credential, String> {
        let password = self.keyring.load_password(&self.account_id, PROTOCOL_IMAP).await?;
        Ok(Credential::Password(password))
    }

    async fn smtp_credential(&self) -> Result<Credential, String> {
        let password = self.keyring.load_password(&self.account_id, PROTOCOL_SMTP).await?;
        Ok(Credential::Password(password))
    }
}

// ---------------------------------------------------------------------------
// Add/edit dialog
// ---------------------------------------------------------------------------

/// Opens the add-account (or edit-account) dialog for one manual IMAP/SMTP
/// account, anchored to `anchor`'s window when one exists (tests have no
/// window). `existing` is `None` for a new account. Passwords are never
/// loaded back out of the keyring for display - on edit the password fields
/// start blank, and blank means "keep the stored secrets", so changing
/// server settings never requires re-typing the password.
///
/// On a successful save (`on_saved` fires on the UI thread with the account
/// id, after both the keyring write and the `settings.json` write have
/// landed) the caller connects or reconnects the account.
pub fn show_add_account_dialog(
    anchor: &gtk::Widget,
    worker: Rc<Worker>,
    app_config: Rc<RefCell<AppConfig>>,
    keyring: Arc<dyn KeyringStore>,
    existing: Option<OtherAccount>,
    on_saved: Rc<dyn Fn(&str)>,
) {
    let is_edit = existing.is_some();
    let dialog = {
        let mut builder = gtk::Window::builder()
            .modal(true)
            .title(if is_edit { "Edit mail account" } else { "Add mail account" })
            .default_width(773)
            .default_height(640);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    // -- form fields --
    let name_entry = gtk::Entry::builder().placeholder_text("Display name").build();
    let email_entry = gtk::Entry::builder().placeholder_text("name@example.com").build();

    let imap_host_entry = gtk::Entry::builder().placeholder_text("imap.example.com").build();
    let imap_port_spin = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    imap_port_spin.set_value(993.0);
    let imap_username_entry = gtk::Entry::builder().placeholder_text("username").build();

    // SMTP defaults to mirroring IMAP (the overwhelmingly common case for
    // self-hosted mail); toggling the switch reveals its own fields.
    let same_server_switch = gtk::Switch::new();
    same_server_switch.set_active(true);
    let smtp_host_entry = gtk::Entry::builder().placeholder_text("smtp.example.com").build();
    let smtp_port_spin = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    smtp_port_spin.set_value(587.0);
    let smtp_username_entry = gtk::Entry::builder().placeholder_text("username").build();
    let smtp_fields_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();

    let same_password_switch = gtk::Switch::new();
    same_password_switch.set_active(true);
    let imap_password_entry = gtk::PasswordEntry::builder().placeholder_text("Password").build();
    let smtp_password_entry = gtk::PasswordEntry::builder().placeholder_text("SMTP password").build();
    let smtp_password_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();

    // Seed an edit with the persisted values (never the passwords).
    if let Some(account) = &existing {
        name_entry.set_text(&account.display_name);
        email_entry.set_text(&account.email);
        imap_host_entry.set_text(&account.imap_host);
        imap_port_spin.set_value(account.imap_port as f64);
        imap_username_entry.set_text(&account.imap_username);
        let same_server = account.smtp_host == account.imap_host && account.smtp_username == account.imap_username;
        same_server_switch.set_active(same_server);
        smtp_host_entry.set_text(&account.smtp_host);
        smtp_port_spin.set_value(account.smtp_port as f64);
        smtp_username_entry.set_text(&account.smtp_username);
        same_password_switch.set_active(account.use_same_password);
    }

    // -- assemble the form --
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
    form.attach(&name_entry, 1, row, 1, 1);
    row += 1;
    form.attach(&field_label("Email address"), 0, row, 1, 1);
    form.attach(&email_entry, 1, row, 1, 1);
    row += 1;
    form.attach(&field_label("IMAP server"), 0, row, 1, 1);
    form.attach(&imap_host_entry, 1, row, 1, 1);
    form.attach(&port_label("Port"), 2, row, 1, 1);
    form.attach(&imap_port_spin, 3, row, 1, 1);
    row += 1;
    form.attach(&field_label("IMAP username"), 0, row, 1, 1);
    form.attach(&imap_username_entry, 1, row, 1, 1);
    row += 1;
    let tls_note = gtk::Label::builder()
        .label("Connections are encrypted (SSL/TLS), like the accounts GNOME Online Accounts provides.")
        .wrap(true)
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();
    form.attach(&tls_note, 1, row, 3, 1);
    row += 1;

    let same_server_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).hexpand(true).build();
    let same_server_label = gtk::Label::builder().label("Use the same server for SMTP").hexpand(true).halign(gtk::Align::Start).build();
    same_server_row.append(&same_server_label);
    same_server_row.append(&same_server_switch);
    form.attach(&same_server_row, 1, row, 3, 1);
    row += 1;

    let smtp_host_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    smtp_host_row.append(&smtp_host_entry);
    smtp_host_row.append(&port_label("Port"));
    smtp_host_row.append(&smtp_port_spin);
    let smtp_username_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    let smtp_username_label = gtk::Label::builder().label("SMTP username").width_request(120).halign(gtk::Align::End).build();
    smtp_username_row.append(&smtp_username_label);
    smtp_username_row.append(&smtp_username_entry);
    smtp_fields_box.append(&smtp_host_row);
    smtp_fields_box.append(&smtp_username_row);
    form.attach(&field_label("SMTP"), 0, row, 1, 1);
    form.attach(&smtp_fields_box, 1, row, 3, 1);
    row += 1;

    let same_password_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).hexpand(true).build();
    let same_password_label = gtk::Label::builder()
        .label("Use the same password for sending")
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();
    same_password_row.append(&same_password_label);
    same_password_row.append(&same_password_switch);
    form.attach(&same_password_row, 1, row, 3, 1);
    row += 1;

    form.attach(&field_label("Password"), 0, row, 1, 1);
    form.attach(&imap_password_entry, 1, row, 3, 1);
    row += 1;
    smtp_password_box.append(&smtp_password_entry);
    form.attach(&smtp_password_box, 1, row, 3, 1);
    row += 1;

    let error_label = gtk::Label::builder().halign(gtk::Align::Start).css_classes(["error"]).wrap(true).visible(false).build();
    form.attach(&error_label, 0, row, 4, 1);

    // SMTP mirrors IMAP unless told otherwise.
    for (slave, master) in [(&smtp_host_entry, &imap_host_entry), (&smtp_username_entry, &imap_username_entry)] {
        let slave = slave.clone();
        let master = master.clone();
        let same_server_switch = same_server_switch.clone();
        master.connect_changed(move |entry| {
            if same_server_switch.is_active() {
                slave.set_text(&entry.text());
            }
        });
    }
    same_server_switch.connect_state_set({
        let smtp_fields_box = smtp_fields_box.clone();
        move |_switch, state| {
            smtp_fields_box.set_sensitive(state);
            glib::Propagation::Proceed
        }
    });
    same_password_switch.connect_state_set({
        let smtp_password_box = smtp_password_box.clone();
        move |_switch, state| {
            smtp_password_box.set_visible(state);
            glib::Propagation::Proceed
        }
    });
    // `connect_state_set` only fires on user interaction, not the initial
    // `set_active` seed above - sync the dependent fields' visibility once
    // up front so an edit dialog opened with "same server"/"same password"
    // off starts with the right fields shown.
    smtp_fields_box.set_sensitive(same_server_switch.is_active());
    smtp_password_box.set_visible(same_password_switch.is_active());

    // -- footer: Cancel / Save --
    let footer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let cancel_button = gtk::Button::builder().label("Cancel").build();
    let save_button = gtk::Button::builder()
        .label(if is_edit { "Save" } else { "Add account" })
        .css_classes(["suggested-action"])
        .build();
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
        // These three are also needed after `save` is defined below
        // (`save_button.connect_clicked`, `email_entry.connect_activate`,
        // `dialog.present()`), so `save`'s `move` closure gets its own
        // clones rather than moving the outer bindings out from under them.
        let save_button = save_button.clone();
        let email_entry = email_entry.clone();
        let dialog = dialog.clone();
        move || {
            let name = name_entry.text().trim().to_string();
            let email = email_entry.text().trim().to_string();
            let imap_host = imap_host_entry.text().trim().to_string();
            let imap_port = imap_port_spin.value_as_int().clamp(1, 65535) as u16;
            let imap_username = imap_username_entry.text().trim().to_string();
            let same_server = same_server_switch.is_active();
            let smtp_host = if same_server { imap_host.clone() } else { smtp_host_entry.text().trim().to_string() };
            let smtp_port = smtp_port_spin.value_as_int().clamp(1, 65535) as u16;
            let smtp_username = if same_server {
                imap_username.clone()
            } else {
                smtp_username_entry.text().trim().to_string()
            };
            let use_same_password = same_password_switch.is_active();
            let imap_password = imap_password_entry.text().to_string();
            let smtp_password = if use_same_password {
                imap_password.clone()
            } else {
                smtp_password_entry.text().to_string()
            };

            // -- validate --
            let mut problems: Vec<String> = Vec::new();
            if name.is_empty() {
                problems.push("a display name is required".to_string());
            }
            if !crate::recipient_entry::is_plausible_address(&email) {
                problems.push("a valid email address is required".to_string());
            }
            if imap_host.is_empty() {
                problems.push("an IMAP server is required".to_string());
            }
            if imap_username.is_empty() {
                problems.push("an IMAP username is required".to_string());
            }
            if !same_server && smtp_host.is_empty() {
                problems.push("an SMTP server is required".to_string());
            }
            if !same_server && smtp_username.is_empty() {
                problems.push("an SMTP username is required".to_string());
            }
            // Blank passwords are only allowed when editing (keep the stored
            // secrets); a new account must set them.
            if imap_password.is_empty() && existing.is_none() {
                problems.push("a password is required".to_string());
            }
            if !use_same_password && smtp_password.is_empty() && existing.is_none() {
                problems.push("an SMTP password is required".to_string());
            }
            if !problems.is_empty() {
                error_label.set_label(&problems.join("\n"));
                error_label.set_visible(true);
                return;
            }
            error_label.set_visible(false);

            let id = existing
                .as_ref()
                .map(|a| a.id.clone())
                .unwrap_or_else(|| format!("{OTHER_ACCOUNT_ID_PREFIX}{}", uuid::Uuid::new_v4()));
            let account = OtherAccount {
                id,
                display_name: name,
                email,
                imap_host,
                imap_port,
                // The mail engine always connects IMAP over TLS (see
                // `lookout-mail`'s `connection.rs`) and drives SMTP TLS by
                // port (465 = implicit, otherwise STARTTLS); the flags are
                // stored for display parity with GOA accounts.
                imap_use_tls: true,
                imap_username,
                smtp_host,
                smtp_port,
                smtp_use_tls: true,
                smtp_username,
                use_same_password,
            };

            // Keyring writes happen on the worker thread (D-Bus I/O). The
            // config mutation + save stay on the UI thread, like the
            // identities dialog, and only run once the secrets are stored -
            // a failed keyring write leaves settings.json untouched so the
            // account isn't half-configured. Blank password fields (the
            // edit-without-changing-the-password case) skip that slot's
            // write entirely, leaving the existing keyring entry untouched.
            save_button.set_sensitive(false);
            let keyring = keyring.clone();
            let app_config = app_config.clone();
            let dialog = dialog.clone();
            let on_saved = on_saved.clone();
            let account_for_worker = account.clone();
            let (result_tx, result_rx) = async_channel::bounded(1);
            let account_id = account.id.clone();
            let secrets: (Option<String>, Option<String>) = (
                if imap_password.is_empty() { None } else { Some(imap_password) },
                if smtp_password.is_empty() { None } else { Some(smtp_password) },
            );
            worker.spawn(async move {
                let result = async {
                    if let Some(password) = &secrets.0 {
                        keyring.store_password(&AccountId(account_for_worker.id.clone()), PROTOCOL_IMAP, password).await?;
                    }
                    if let Some(password) = &secrets.1 {
                        keyring.store_password(&AccountId(account_for_worker.id.clone()), PROTOCOL_SMTP, password).await?;
                    }
                    Ok::<_, String>(())
                }
                .await;
                let _ = result_tx.send(result).await;
            });
            // Fresh clones for this async block specifically: `save_button`
            // and `error_label` are also used synchronously above (and on
            // every future call of this `Fn` closure), so moving the
            // closure's own captured fields into the async block - rather
            // than a clone made just for it - would make `save` callable
            // only once.
            let save_button = save_button.clone();
            let error_label = error_label.clone();
            glib::spawn_future_local(async move {
                let Ok(result) = result_rx.recv().await else { return };
                match result {
                    Ok(()) => {
                        app_config.borrow_mut().upsert_other_account(account.clone());
                        crate::app_config::save(&app_config.borrow());
                        dialog.close();
                        on_saved(&account_id);
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
    email_entry.connect_activate(move |_| save_for_activate());

    dialog.present();
}

/// A right-aligned field label for the two-column form grid.
fn field_label(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).halign(gtk::Align::End).build()
}

/// A small "Port" caption for the grid's port columns.
fn port_label(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).css_classes(["dim-label"]).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory keyring for tests: a map of `(account, protocol)` to the
    /// stored password, behind a mutex for the `Send + Sync` trait bound.
    #[derive(Default)]
    struct FakeKeyring {
        secrets: Mutex<HashMap<(String, String), String>>,
    }

    #[async_trait]
    impl KeyringStore for FakeKeyring {
        async fn load_password(&self, account_id: &AccountId, protocol: &str) -> Result<String, String> {
            self.secrets
                .lock()
                .unwrap()
                .get(&(account_id.0.clone(), protocol.to_string()))
                .cloned()
                .ok_or_else(|| "missing".to_string())
        }

        async fn store_password(&self, account_id: &AccountId, protocol: &str, password: &str) -> Result<(), String> {
            self.secrets.lock().unwrap().insert((account_id.0.clone(), protocol.to_string()), password.to_string());
            Ok(())
        }

        async fn delete_account(&self, account_id: &AccountId) -> Result<(), String> {
            self.secrets.lock().unwrap().retain(|(id, _), _| id != &account_id.0);
            Ok(())
        }
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(fut)
    }

    #[test]
    fn provider_reads_the_matching_protocol_slots() {
        let keyring = Arc::new(FakeKeyring::default());
        let account_id = AccountId(format!("{OTHER_ACCOUNT_ID_PREFIX}t"));
        block_on(keyring.store_password(&account_id, PROTOCOL_IMAP, "imap-secret")).unwrap();
        block_on(keyring.store_password(&account_id, PROTOCOL_SMTP, "smtp-secret")).unwrap();

        let provider = OtherCredentialProvider::new(account_id.clone(), keyring.clone());
        block_on(async {
            match provider.imap_credential().await.unwrap() {
                Credential::Password(p) => assert_eq!(p, "imap-secret"),
                _ => panic!("expected a password credential"),
            }
            match provider.smtp_credential().await.unwrap() {
                Credential::Password(p) => assert_eq!(p, "smtp-secret"),
                _ => panic!("expected a password credential"),
            }
            // The slots are independent.
            assert!(provider.imap_credential().await.is_ok());
            keyring.delete_account(&account_id).await.unwrap();
            assert!(provider.imap_credential().await.is_err());
            assert!(provider.smtp_credential().await.is_err());
        });
    }

    #[test]
    fn provider_uses_distinct_passwords_when_stored_so() {
        let keyring = Arc::new(FakeKeyring::default());
        let account_id = AccountId(format!("{OTHER_ACCOUNT_ID_PREFIX}t2"));
        block_on(keyring.store_password(&account_id, PROTOCOL_IMAP, "a")).unwrap();
        block_on(keyring.store_password(&account_id, PROTOCOL_SMTP, "b")).unwrap();
        let provider = OtherCredentialProvider::new(account_id, keyring);
        block_on(async {
            match provider.imap_credential().await.unwrap() {
                Credential::Password(p) => assert_eq!(p, "a"),
                _ => panic!(),
            }
            match provider.smtp_credential().await.unwrap() {
                Credential::Password(p) => assert_eq!(p, "b"),
                _ => panic!(),
            }
        });
    }

    #[test]
    fn keyring_error_messages_are_human_readable() {
        assert!(keyring_error(secret_service::Error::Unavailable).contains("keyring"));
        assert!(keyring_error(secret_service::Error::NoResult).contains("collection"));
        assert!(!keyring_error(secret_service::Error::Locked).is_empty());
    }
}
