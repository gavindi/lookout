//! Importing Evolution's mail accounts as manual IMAP/SMTP accounts
//! (EDS_TODO.md, Phase 3).
//!
//! `lookout-eds` reads the accounts (E3.1). This module turns each one into a
//! pre-filled [`OtherAccountDraft`] and recognises the ones Lookout already
//! has (E3.2), and looks up the passwords Evolution keeps in the keyring
//! (E3.3). [`show_import_dialog`] is the import UI (E3.4).
//!
//! Everything here fails soft: no EDS, no keyring or a locked item means
//! "nothing to import" or "ask for the password as usual", never an error.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use async_trait::async_trait;
use gtk::glib;
use lookout_core::AccountId;
use lookout_eds::{Eds, EvolutionMailAccount, MailSecurity};
use secret_service::{EncryptionType, SecretService};

use crate::app_config::{AppConfig, OtherAccount};
use crate::other_accounts::{account_problems, new_other_account_id, show_add_account_dialog, KeyringStore, PROTOCOL_IMAP, PROTOCOL_SMTP};
use crate::worker::Worker;

/// The SMTP port a draft gets when Evolution has no usable transport - the
/// add-account dialog's own default.
const DEFAULT_SMTP_PORT: u16 = 587;

/// The libsecret schema EDS stores source passwords under, and the attribute
/// that names the source (`e-secret-store.c`).
const EVOLUTION_SECRET_SCHEMA: &str = "org.gnome.Evolution.Data.Source";
const EVOLUTION_SOURCE_UID_ATTRIBUTE: &str = "e-source-uid";

/// The `xdg:schema` attribute libsecret adds to every item it stores.
const XDG_SCHEMA_ATTRIBUTE: &str = "xdg:schema";

// ---------------------------------------------------------------------------
// Drafts (E3.2)
// ---------------------------------------------------------------------------

/// An [`OtherAccount`] pre-filled from Evolution, without an id: a new
/// `other:<uuid>` is generated when it is saved ([`Self::into_new_account`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OtherAccountDraft {
    pub display_name: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_use_tls: bool,
    pub imap_username: String,
    /// Blank when Evolution has no usable transport; the user fills it in.
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_use_tls: bool,
    pub smtp_username: String,
    pub use_same_password: bool,
    /// Evolution's account source, whose keyring item holds the IMAP password.
    pub imap_source_uid: String,
    /// Evolution's transport source, whose keyring item holds the SMTP
    /// password. `None` when SMTP was left blank.
    pub smtp_source_uid: Option<String>,
    /// How Evolution secures each connection, for [`Self::connection_warning`].
    pub imap_security: MailSecurity,
    pub smtp_security: Option<MailSecurity>,
}

impl OtherAccountDraft {
    /// The account to save, under a fresh `other:<uuid>` id.
    pub fn into_new_account(self) -> OtherAccount {
        OtherAccount {
            id: new_other_account_id(),
            display_name: self.display_name,
            email: self.email,
            imap_host: self.imap_host,
            imap_port: self.imap_port,
            imap_use_tls: self.imap_use_tls,
            imap_username: self.imap_username,
            smtp_host: self.smtp_host,
            smtp_port: self.smtp_port,
            smtp_use_tls: self.smtp_use_tls,
            smtp_username: self.smtp_username,
            use_same_password: self.use_same_password,
        }
    }

    /// Whether Lookout already has this account: one of its manual accounts
    /// uses the same IMAP host (case-insensitively) and user, or any account
    /// (manual, or a GOA or Microsoft 365 one passed in `known_addresses`) has
    /// the same address (case-insensitively).
    pub fn is_already_present(&self, other_accounts: &[OtherAccount], known_addresses: &[&str]) -> bool {
        let email = self.email.trim();
        let same_address = |address: &str| !email.is_empty() && address.trim().eq_ignore_ascii_case(email);
        other_accounts.iter().any(|account| {
            same_address(&account.email) || (account.imap_host.trim().eq_ignore_ascii_case(self.imap_host.trim()) && account.imap_username.trim() == self.imap_username.trim())
        }) || known_addresses.iter().any(|address| same_address(address))
    }

    /// Why the account may not work in Lookout the way it does in Evolution,
    /// or `None`. The mail engine always connects IMAP with implicit TLS and
    /// picks SMTP's TLS mode by port (465 implicit, otherwise STARTTLS), so
    /// IMAP over STARTTLS or without encryption won't connect, and SMTP's
    /// mode has to agree with its port.
    pub fn connection_warning(&self) -> Option<String> {
        match self.imap_security {
            MailSecurity::Tls => {}
            MailSecurity::StartTls => {
                return Some(format!(
                    "Evolution uses STARTTLS for IMAP (port {}), but Lookout connects with SSL/TLS only, so it may not connect",
                    self.imap_port
                ));
            }
            MailSecurity::None => return Some("Evolution reads mail without encryption, but Lookout connects with SSL/TLS only, so it may not connect".to_string()),
        }
        match self.smtp_security? {
            MailSecurity::Tls if self.smtp_port != 465 => Some(format!(
                "Evolution sends with SSL/TLS on port {}, but Lookout uses SSL/TLS only on port 465, so sending may fail",
                self.smtp_port
            )),
            MailSecurity::StartTls if self.smtp_port == 465 => {
                Some("Evolution sends with STARTTLS on port 465, but Lookout uses SSL/TLS on that port, so sending may fail".to_string())
            }
            MailSecurity::None => Some("Evolution sends without encryption, but Lookout always encrypts, so sending may fail".to_string()),
            _ => None,
        }
    }
}

/// Fills the [`OtherAccount`] fields from an Evolution account.
///
/// - The name is the identity's sender name, else the account's name in
///   Evolution; the address is the identity's, or blank.
/// - Each `*_use_tls` flag is true for STARTTLS and implicit TLS and false
///   only for a plain connection. Like the add-account dialog, these flags are
///   descriptive: the mail engine always connects IMAP with implicit TLS, and
///   picks SMTP's TLS mode by port (465 implicit, otherwise STARTTLS).
/// - With no usable transport, SMTP is left blank (port 587, TLS on) for the
///   user to fill in.
/// - `use_same_password` starts on, as in the dialog; [`apply_found_passwords`]
///   turns it off when Evolution stored two different passwords.
pub(crate) fn to_other_account_draft(account: &EvolutionMailAccount) -> OtherAccountDraft {
    let uses_tls = |security: MailSecurity| security != MailSecurity::None;
    let smtp = account.smtp.as_ref();
    OtherAccountDraft {
        display_name: account.name.clone().unwrap_or_else(|| account.account_name.clone()),
        email: account.address.clone().unwrap_or_default(),
        imap_host: account.imap.host.clone(),
        imap_port: account.imap.port,
        imap_use_tls: uses_tls(account.imap.security),
        imap_username: account.imap.user.clone(),
        smtp_host: smtp.map(|s| s.host.clone()).unwrap_or_default(),
        smtp_port: smtp.map_or(DEFAULT_SMTP_PORT, |s| s.port),
        smtp_use_tls: smtp.is_none_or(|s| uses_tls(s.security)),
        smtp_username: smtp.map(|s| s.user.clone()).unwrap_or_default(),
        use_same_password: true,
        imap_source_uid: account.imap.source_uid.clone(),
        smtp_source_uid: smtp.map(|s| s.source_uid.clone()),
        imap_security: account.imap.security,
        smtp_security: smtp.map(|s| s.security),
    }
}

/// Evolution's importable accounts on this desktop, as drafts. Empty when EDS
/// isn't running (it is never started), is an unknown version, or fails.
pub(crate) async fn evolution_account_drafts() -> Vec<OtherAccountDraft> {
    let eds = match Eds::connect().await {
        Ok(Some(eds)) => eds,
        Ok(None) => return Vec::new(),
        Err(e) => {
            tracing::debug!("Evolution import: EDS unavailable: {e}");
            return Vec::new();
        }
    };
    match eds.mail_accounts().await {
        Ok(accounts) => accounts.iter().map(to_other_account_draft).collect(),
        Err(e) => {
            tracing::debug!("Evolution import: couldn't list mail accounts: {e}");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Passwords (E3.3)
// ---------------------------------------------------------------------------

/// Read access to the passwords Evolution keeps for its sources. Behind a
/// trait so the import is testable without a Secret Service daemon;
/// [`EvolutionKeyring`] in production.
#[async_trait]
pub(crate) trait EvolutionSecrets: Send + Sync {
    /// The password stored for one Evolution source, or `None` when there is
    /// none, it is locked, or the keyring can't be reached. Never prompts.
    async fn password(&self, source_uid: &str) -> Option<String>;
}

/// The production lookup, through the same `secret-service` client as
/// [`crate::other_accounts::SecretServiceKeyring`]. Only reads: Evolution's
/// items are never copied, moved or changed.
pub(crate) struct EvolutionKeyring;

#[async_trait]
impl EvolutionSecrets for EvolutionKeyring {
    async fn password(&self, source_uid: &str) -> Option<String> {
        match lookup_evolution_password(source_uid).await {
            Ok(password) => password,
            // Expected in the Flatpak, which isn't granted the keyring.
            Err(e) => {
                tracing::debug!("Evolution import: keyring lookup for {source_uid} failed: {e}");
                None
            }
        }
    }
}

async fn lookup_evolution_password(source_uid: &str) -> Result<Option<String>, secret_service::Error> {
    let service = SecretService::connect(EncryptionType::Dh).await?;
    // EDS's schema doesn't match on its name (items written before libsecret
    // carry no `xdg:schema`), so search by the source UID alone, as EDS does,
    // and check the schema of what comes back.
    let attributes = HashMap::from([(EVOLUTION_SOURCE_UID_ATTRIBUTE, source_uid)]);
    let found = service.search_items(attributes).await?;
    // Locked items are left alone: unlocking would prompt the user.
    for item in found.unlocked {
        if !is_evolution_item(&item.get_attributes().await?) {
            continue;
        }
        if let Some(password) = password_text(item.get_secret().await?) {
            return Ok(Some(password));
        }
    }
    Ok(None)
}

/// Whether a keyring item with `e-source-uid` is Evolution's: its `xdg:schema`
/// is EDS's, or it has none (an item from before libsecret).
fn is_evolution_item(attributes: &HashMap<String, String>) -> bool {
    attributes.get(XDG_SCHEMA_ATTRIBUTE).is_none_or(|schema| schema == EVOLUTION_SECRET_SCHEMA)
}

/// The secret as a password: non-empty UTF-8 text.
fn password_text(secret: Vec<u8>) -> Option<String> {
    String::from_utf8(secret).ok().filter(|password| !password.is_empty())
}

/// The passwords found in Evolution's keyring items for one draft.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FoundPasswords {
    pub imap: Option<String>,
    pub smtp: Option<String>,
}

impl FoundPasswords {
    /// The `(imap, smtp)` passwords to store, or `None` when the IMAP password
    /// wasn't found and the user has to be asked as usual. SMTP falls back to
    /// the IMAP password, as the dialog's "same password" switch does: a
    /// manual account always needs both keyring slots.
    pub fn slots(&self) -> Option<(&str, &str)> {
        let imap = self.imap.as_deref()?;
        Some((imap, self.smtp.as_deref().unwrap_or(imap)))
    }
}

/// Looks up the IMAP password (the account source's item) and the SMTP
/// password (the transport's item, when SMTP was imported).
pub(crate) async fn find_passwords(secrets: &dyn EvolutionSecrets, draft: &OtherAccountDraft) -> FoundPasswords {
    let imap = secrets.password(&draft.imap_source_uid).await;
    let smtp = match &draft.smtp_source_uid {
        Some(uid) => secrets.password(uid).await,
        None => None,
    };
    FoundPasswords { imap, smtp }
}

/// Records whether the found passwords differ, so the edit dialog later
/// shows separate fields for them.
pub(crate) fn apply_found_passwords(draft: &mut OtherAccountDraft, found: &FoundPasswords) {
    draft.use_same_password = found.slots().is_none_or(|(imap, smtp)| imap == smtp);
}

/// Stores Lookout's own copies of the found passwords in the account's two
/// keyring slots, the same way the add-account dialog does. `Ok(false)` when
/// there was nothing to store, so the caller asks for the password instead.
pub(crate) async fn store_found_passwords(keyring: &dyn KeyringStore, account_id: &AccountId, found: &FoundPasswords) -> Result<bool, String> {
    let Some((imap, smtp)) = found.slots() else { return Ok(false) };
    keyring.store_password(account_id, PROTOCOL_IMAP, imap).await?;
    keyring.store_password(account_id, PROTOCOL_SMTP, smtp).await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// The import dialog (E3.4)
// ---------------------------------------------------------------------------

/// The note under the account list: passwords come along only when the
/// keyring lets them.
const PASSWORD_NOTE: &str =
    "Saved passwords are copied from Evolution's keyring only when it is unlocked and Lookout can reach it. Otherwise you'll be asked for each account's password.";

/// The subtitle of an account Lookout already has.
const ALREADY_PRESENT_SUBTITLE: &str = "Already in Lookout";

/// Whether any draft is missing from Lookout - the condition for offering
/// "Import from Evolution…" at all.
pub(crate) fn any_importable(drafts: &[OtherAccountDraft], other_accounts: &[OtherAccount], known_addresses: &[&str]) -> bool {
    drafts.iter().any(|draft| !draft.is_already_present(other_accounts, known_addresses))
}

/// The dialog's account list: one row per draft with a check button, ticked
/// unless Lookout already has the account. Rows for accounts already present
/// say so, and their check button stays off and can't be ticked; the others
/// show a [`OtherAccountDraft::connection_warning`] when there is one, or the
/// IMAP server and user.
pub(crate) fn build_import_list(
    drafts: Vec<OtherAccountDraft>,
    other_accounts: &[OtherAccount],
    known_addresses: &[&str],
) -> (adw::PreferencesGroup, Vec<(gtk::CheckButton, OtherAccountDraft)>) {
    let group = adw::PreferencesGroup::builder().title("Evolution mail accounts").description(PASSWORD_NOTE).build();
    let mut rows = Vec::new();
    for draft in drafts {
        let present = draft.is_already_present(other_accounts, known_addresses);
        let check = gtk::CheckButton::builder().active(!present).sensitive(!present).valign(gtk::Align::Center).build();
        let title = if draft.email.is_empty() {
            draft.display_name.clone()
        } else {
            format!("{} <{}>", draft.display_name, draft.email)
        };
        let subtitle = if present {
            ALREADY_PRESENT_SUBTITLE.to_string()
        } else {
            draft.connection_warning().unwrap_or_else(|| format!("{} · {}", draft.imap_host, draft.imap_username))
        };
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .use_markup(false)
            .activatable_widget(&check)
            .sensitive(!present)
            .build();
        row.add_prefix(&check);
        group.add(&row);
        rows.push((check, draft));
    }
    (group, rows)
}

/// Opens "Import from Evolution" over `anchor`'s window, listing `drafts`.
/// Each ticked account whose passwords are found (when copying is allowed)
/// and whose settings pass the add-account dialog's checks is saved straight
/// away, through the same keyring-then-`settings.json` order as that dialog;
/// `on_saved` then fires with its id, as it does for the dialog. Every other
/// ticked account opens the add-account dialog pre-filled, one after another,
/// so the user can enter the password or fix what's missing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn show_import_dialog(
    anchor: &gtk::Widget,
    worker: Rc<Worker>,
    app_config: Rc<RefCell<AppConfig>>,
    keyring: Arc<dyn KeyringStore>,
    drafts: Vec<OtherAccountDraft>,
    known_addresses: Vec<String>,
    on_saved: Rc<dyn Fn(&str)>,
) {
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Import from Evolution").default_width(640).default_height(520);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let (list, rows) = {
        let known: Vec<&str> = known_addresses.iter().map(String::as_str).collect();
        build_import_list(drafts, &app_config.borrow().other_accounts, &known)
    };
    let copy_passwords_row = adw::SwitchRow::builder()
        .title("Copy saved passwords")
        .subtitle("Read each account's password from Evolution's keyring and store Lookout's own copy")
        .active(true)
        .build();
    crate::privacy::mark_ui(&copy_passwords_row);
    let options = adw::PreferencesGroup::new();
    options.add(&copy_passwords_row);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&list);
    content.append(&options);

    let footer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let cancel_button = gtk::Button::builder().label("Cancel").build();
    let import_button = gtk::Button::builder().label("Import").css_classes(["suggested-action"]).build();
    footer.append(&cancel_button);
    footer.append(&import_button);

    let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&content)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    root.append(&scroller);
    root.append(&footer);
    dialog.set_child(Some(&root));

    let rows = Rc::new(rows);
    // Import is only possible with at least one account ticked.
    let sync_import_button = {
        let rows = rows.clone();
        let import_button = import_button.clone();
        move || import_button.set_sensitive(rows.iter().any(|(check, _)| check.is_active()))
    };
    sync_import_button();
    for (check, _) in rows.iter() {
        let sync_import_button = sync_import_button.clone();
        check.connect_toggled(move |_| sync_import_button());
    }

    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }

    let anchor = anchor.clone();
    let dialog_for_import = dialog.clone();
    import_button.connect_clicked(move |button| {
        button.set_sensitive(false);
        let chosen: Vec<OtherAccountDraft> = rows.iter().filter(|(check, _)| check.is_active()).map(|(_, draft)| draft.clone()).collect();
        let copy_passwords = copy_passwords_row.is_active();
        let (result_tx, result_rx) = async_channel::bounded(1);
        let keyring_for_worker = keyring.clone();
        worker.spawn(async move {
            let _ = result_tx
                .send(import_accounts(chosen, copy_passwords, &EvolutionKeyring, keyring_for_worker.as_ref()).await)
                .await;
        });

        let anchor = anchor.clone();
        let worker = worker.clone();
        let app_config = app_config.clone();
        let keyring = keyring.clone();
        let on_saved = on_saved.clone();
        let dialog = dialog_for_import.clone();
        glib::spawn_future_local(async move {
            let Ok(imported) = result_rx.recv().await else { return };
            let mut saved = Vec::new();
            let mut needs_dialog = VecDeque::new();
            for ImportedAccount { account, passwords_stored } in imported {
                if passwords_stored {
                    saved.push(account.id.clone());
                    app_config.borrow_mut().upsert_other_account(account);
                } else {
                    needs_dialog.push_back(account);
                }
            }
            if !saved.is_empty() {
                crate::app_config::save(&app_config.borrow());
            }
            dialog.close();
            for id in &saved {
                on_saved(id);
            }
            show_prefilled_dialogs(anchor, worker, app_config, keyring, needs_dialog, on_saved);
        });
    });

    dialog.present();
}

/// One ticked account after the worker-thread half of the import.
struct ImportedAccount {
    /// Under its new `other:<uuid>` id, with `use_same_password` set from the
    /// passwords found.
    account: OtherAccount,
    /// Whether Lookout's keyring slots now hold its passwords, so it can be
    /// saved without the dialog.
    passwords_stored: bool,
}

/// Finds and stores each draft's passwords (on the worker thread: keyring
/// D-Bus I/O). Passwords are stored only for an account that passes the
/// add-account dialog's checks with SMTP included, since an account that
/// needs the dialog gets a new id there. A failed store removes whatever
/// half of it landed and leaves the account to the dialog.
async fn import_accounts(drafts: Vec<OtherAccountDraft>, copy_passwords: bool, secrets: &dyn EvolutionSecrets, keyring: &dyn KeyringStore) -> Vec<ImportedAccount> {
    let mut imported = Vec::new();
    for mut draft in drafts {
        let found = if copy_passwords {
            find_passwords(secrets, &draft).await
        } else {
            FoundPasswords::default()
        };
        apply_found_passwords(&mut draft, &found);
        let account = draft.into_new_account();
        let account_id = AccountId(account.id.clone());
        let passwords_stored = account_problems(&account, true).is_empty()
            && match store_found_passwords(keyring, &account_id, &found).await {
                Ok(stored) => stored,
                Err(e) => {
                    tracing::warn!("Evolution import: couldn't store the passwords for {}: {e}", account.email);
                    let _ = keyring.delete_account(&account_id).await;
                    false
                }
            };
        imported.push(ImportedAccount { account, passwords_stored });
    }
    imported
}

/// Opens the add-account dialog pre-filled with each account in turn, the next
/// once the previous one closes (saved or cancelled).
fn show_prefilled_dialogs(
    anchor: gtk::Widget,
    worker: Rc<Worker>,
    app_config: Rc<RefCell<AppConfig>>,
    keyring: Arc<dyn KeyringStore>,
    mut queue: VecDeque<OtherAccount>,
    on_saved: Rc<dyn Fn(&str)>,
) {
    let Some(account) = queue.pop_front() else { return };
    let dialog = show_add_account_dialog(&anchor, worker.clone(), app_config.clone(), keyring.clone(), None, Some(account), on_saved.clone());
    let next = RefCell::new(Some((anchor, worker, app_config, keyring, queue, on_saved)));
    dialog.connect_close_request(move |_| {
        if let Some((anchor, worker, app_config, keyring, queue, on_saved)) = next.borrow_mut().take() {
            // After this dialog is gone, so the next one isn't stacked on it.
            glib::idle_add_local_once(move || show_prefilled_dialogs(anchor, worker, app_config, keyring, queue, on_saved));
        }
        glib::Propagation::Proceed
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_config::OTHER_ACCOUNT_ID_PREFIX;
    use lookout_eds::MailServer;
    use std::sync::Mutex;

    fn server(source_uid: &str, host: &str, port: u16, user: &str, security: MailSecurity) -> MailServer {
        MailServer {
            source_uid: source_uid.to_string(),
            host: host.to_string(),
            port,
            user: user.to_string(),
            security,
        }
    }

    fn evolution_account() -> EvolutionMailAccount {
        EvolutionMailAccount {
            account_name: "Work".to_string(),
            name: Some("Wren Example".to_string()),
            address: Some("wren@example.org".to_string()),
            imap: server("acct-uid", "imap.example.org", 993, "wren", MailSecurity::Tls),
            smtp: Some(server("transport-uid", "smtp.example.org", 587, "wren.submit", MailSecurity::StartTls)),
        }
    }

    fn other_account(email: &str, imap_host: &str, imap_username: &str) -> OtherAccount {
        let mut draft = to_other_account_draft(&evolution_account());
        draft.email = email.to_string();
        draft.imap_host = imap_host.to_string();
        draft.imap_username = imap_username.to_string();
        draft.into_new_account()
    }

    #[test]
    fn draft_fields_come_from_the_account_identity_and_transport() {
        let draft = to_other_account_draft(&evolution_account());
        assert_eq!(
            draft,
            OtherAccountDraft {
                display_name: "Wren Example".to_string(),
                email: "wren@example.org".to_string(),
                imap_host: "imap.example.org".to_string(),
                imap_port: 993,
                imap_use_tls: true,
                imap_username: "wren".to_string(),
                smtp_host: "smtp.example.org".to_string(),
                smtp_port: 587,
                smtp_use_tls: true,
                smtp_username: "wren.submit".to_string(),
                use_same_password: true,
                imap_source_uid: "acct-uid".to_string(),
                smtp_source_uid: Some("transport-uid".to_string()),
                imap_security: MailSecurity::Tls,
                smtp_security: Some(MailSecurity::StartTls),
            }
        );
    }

    #[test]
    fn a_nameless_identity_falls_back_to_the_account_name() {
        let mut account = evolution_account();
        account.name = None;
        account.address = None;
        let draft = to_other_account_draft(&account);
        assert_eq!(draft.display_name, "Work");
        assert_eq!(draft.email, "");
    }

    #[test]
    fn a_missing_transport_leaves_smtp_blank() {
        let mut account = evolution_account();
        account.smtp = None;
        let draft = to_other_account_draft(&account);
        assert_eq!((draft.smtp_host.as_str(), draft.smtp_username.as_str()), ("", ""));
        assert_eq!(draft.smtp_port, DEFAULT_SMTP_PORT);
        assert!(draft.smtp_use_tls);
        assert_eq!(draft.smtp_source_uid, None);
    }

    #[test]
    fn security_methods_map_to_the_tls_flags() {
        for (security, expected) in [(MailSecurity::Tls, true), (MailSecurity::StartTls, true), (MailSecurity::None, false)] {
            let mut account = evolution_account();
            account.imap.security = security;
            account.smtp.as_mut().unwrap().security = security;
            let draft = to_other_account_draft(&account);
            assert_eq!((draft.imap_use_tls, draft.smtp_use_tls), (expected, expected), "{security:?}");
        }
    }

    #[test]
    fn a_saved_draft_gets_a_new_other_account_id() {
        let first = to_other_account_draft(&evolution_account()).into_new_account();
        let second = to_other_account_draft(&evolution_account()).into_new_account();
        assert!(first.id.starts_with(OTHER_ACCOUNT_ID_PREFIX), "{}", first.id);
        assert_ne!(first.id, second.id);
        assert_eq!(first.email, "wren@example.org");
        assert_eq!(first.smtp_username, "wren.submit");
    }

    #[test]
    fn duplicates_match_on_address_or_imap_host_and_user() {
        let draft = to_other_account_draft(&evolution_account());
        assert!(!draft.is_already_present(&[], &[]));
        // The same address, in another case, on a manual account or any other.
        assert!(draft.is_already_present(&[other_account("WREN@Example.org", "elsewhere.example", "x")], &[]));
        assert!(draft.is_already_present(&[], &["Wren@example.ORG"]));
        // The same IMAP host (any case) and user under another address.
        assert!(draft.is_already_present(&[other_account("alias@example.org", "IMAP.example.org", "wren")], &[]));
        // The same host with another user, or another host, is a different account.
        assert!(!draft.is_already_present(&[other_account("alias@example.org", "imap.example.org", "robin")], &["robin@example.org"]));
        assert!(!draft.is_already_present(&[other_account("alias@example.org", "imap.example.net", "wren")], &[]));
    }

    #[test]
    fn a_blank_address_never_matches_by_address() {
        let mut draft = to_other_account_draft(&evolution_account());
        draft.email = String::new();
        assert!(!draft.is_already_present(&[other_account("", "imap.example.net", "x")], &[""]));
    }

    /// Evolution's keyring, as a map of source UID to password.
    struct FakeEvolutionSecrets(HashMap<&'static str, &'static str>);

    #[async_trait]
    impl EvolutionSecrets for FakeEvolutionSecrets {
        async fn password(&self, source_uid: &str) -> Option<String> {
            self.0.get(source_uid).map(|p| p.to_string())
        }
    }

    /// Lookout's keyring: `(account, protocol)` to password.
    #[derive(Default)]
    struct FakeKeyring(Mutex<HashMap<(String, String), String>>);

    #[async_trait]
    impl KeyringStore for FakeKeyring {
        async fn load_password(&self, account_id: &AccountId, protocol: &str) -> Result<String, String> {
            self.0
                .lock()
                .unwrap()
                .get(&(account_id.0.clone(), protocol.to_string()))
                .cloned()
                .ok_or_else(|| "missing".to_string())
        }

        async fn store_password(&self, account_id: &AccountId, protocol: &str, password: &str) -> Result<(), String> {
            self.0.lock().unwrap().insert((account_id.0.clone(), protocol.to_string()), password.to_string());
            Ok(())
        }

        async fn delete_account(&self, account_id: &AccountId) -> Result<(), String> {
            self.0.lock().unwrap().retain(|(id, _), _| id != &account_id.0);
            Ok(())
        }
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(fut)
    }

    /// Finds the draft's passwords in `evolution`, applies and stores them,
    /// and returns what was stored, the draft and Lookout's keyring.
    fn import(evolution: &[(&'static str, &'static str)], draft: &mut OtherAccountDraft) -> (bool, HashMap<(String, String), String>) {
        let secrets = FakeEvolutionSecrets(evolution.iter().copied().collect());
        let keyring = FakeKeyring::default();
        let stored = block_on(async {
            let found = find_passwords(&secrets, draft).await;
            apply_found_passwords(draft, &found);
            store_found_passwords(&keyring, &AccountId("other:t".to_string()), &found).await.unwrap()
        });
        (stored, keyring.0.into_inner().unwrap())
    }

    fn slot(protocol: &str) -> (String, String) {
        ("other:t".to_string(), protocol.to_string())
    }

    #[test]
    fn found_passwords_are_copied_into_lookouts_own_slots() {
        let mut draft = to_other_account_draft(&evolution_account());
        let (stored, keyring) = import(&[("acct-uid", "imap-secret"), ("transport-uid", "smtp-secret")], &mut draft);
        assert!(stored);
        assert_eq!(keyring[&slot(PROTOCOL_IMAP)], "imap-secret");
        assert_eq!(keyring[&slot(PROTOCOL_SMTP)], "smtp-secret");
        assert!(!draft.use_same_password, "two different passwords need separate fields");
    }

    #[test]
    fn the_same_password_twice_keeps_the_single_password_switch() {
        let mut draft = to_other_account_draft(&evolution_account());
        let (stored, _) = import(&[("acct-uid", "secret"), ("transport-uid", "secret")], &mut draft);
        assert!(stored);
        assert!(draft.use_same_password);
    }

    #[test]
    fn a_missing_smtp_password_reuses_the_imap_one() {
        let mut draft = to_other_account_draft(&evolution_account());
        let (stored, keyring) = import(&[("acct-uid", "imap-secret")], &mut draft);
        assert!(stored);
        assert_eq!(keyring[&slot(PROTOCOL_SMTP)], "imap-secret");
        assert!(draft.use_same_password);
    }

    #[test]
    fn without_the_imap_password_nothing_is_stored() {
        // Sandboxed, locked or missing: the user is asked as usual.
        for evolution in [&[][..], &[("transport-uid", "smtp-secret")][..]] {
            let mut draft = to_other_account_draft(&evolution_account());
            let (stored, keyring) = import(evolution, &mut draft);
            assert!(!stored);
            assert!(keyring.is_empty());
            assert!(draft.use_same_password);
        }
    }

    #[test]
    fn a_blank_transport_is_not_looked_up() {
        let mut account = evolution_account();
        account.smtp = None;
        let mut draft = to_other_account_draft(&account);
        let secrets = FakeEvolutionSecrets(HashMap::from([("acct-uid", "imap-secret"), ("transport-uid", "smtp-secret")]));
        let found = block_on(find_passwords(&secrets, &draft));
        assert_eq!(
            found,
            FoundPasswords {
                imap: Some("imap-secret".to_string()),
                smtp: None
            }
        );
        apply_found_passwords(&mut draft, &found);
        assert!(draft.use_same_password);
    }

    #[test]
    fn only_evolution_items_with_text_secrets_count() {
        let attributes = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
        assert!(is_evolution_item(&attributes(&[
            (XDG_SCHEMA_ATTRIBUTE, EVOLUTION_SECRET_SCHEMA),
            (EVOLUTION_SOURCE_UID_ATTRIBUTE, "u")
        ])));
        assert!(
            is_evolution_item(&attributes(&[(EVOLUTION_SOURCE_UID_ATTRIBUTE, "u")])),
            "items from before libsecret have no schema"
        );
        assert!(!is_evolution_item(&attributes(&[
            (XDG_SCHEMA_ATTRIBUTE, "org.example.Other"),
            (EVOLUTION_SOURCE_UID_ATTRIBUTE, "u")
        ])));
        assert_eq!(password_text(b"secret".to_vec()).as_deref(), Some("secret"));
        assert_eq!(password_text(Vec::new()), None);
        assert_eq!(password_text(vec![0xff, 0xfe]), None);
    }

    #[test]
    fn connection_warnings_follow_the_engines_tls_rules() {
        let draft = |imap: MailSecurity, smtp: Option<(MailSecurity, u16)>| {
            let mut account = evolution_account();
            account.imap.security = imap;
            account.smtp = smtp.map(|(security, port)| server("transport-uid", "smtp.example.org", port, "wren", security));
            to_other_account_draft(&account)
        };
        assert_eq!(draft(MailSecurity::Tls, Some((MailSecurity::StartTls, 587))).connection_warning(), None);
        assert_eq!(draft(MailSecurity::Tls, Some((MailSecurity::Tls, 465))).connection_warning(), None);
        assert_eq!(draft(MailSecurity::Tls, None).connection_warning(), None);
        for (imap, smtp, mentions) in [
            (MailSecurity::StartTls, None, "STARTTLS for IMAP"),
            (MailSecurity::None, None, "without encryption"),
            (MailSecurity::Tls, Some((MailSecurity::Tls, 587)), "port 587"),
            (MailSecurity::Tls, Some((MailSecurity::StartTls, 465)), "port 465"),
            (MailSecurity::Tls, Some((MailSecurity::None, 25)), "sends without encryption"),
        ] {
            let warning = draft(imap, smtp).connection_warning().unwrap_or_else(|| panic!("{imap:?} {smtp:?} should warn"));
            assert!(warning.contains(mentions), "{warning}");
        }
    }

    #[test]
    fn importable_means_any_draft_lookout_lacks() {
        let draft = to_other_account_draft(&evolution_account());
        assert!(any_importable(std::slice::from_ref(&draft), &[], &[]));
        assert!(!any_importable(std::slice::from_ref(&draft), &[], &["wren@example.org"]));
        assert!(!any_importable(&[], &[], &[]));
    }

    #[test]
    fn importing_saves_accounts_with_passwords_and_leaves_the_rest_to_the_dialog() {
        let complete = to_other_account_draft(&evolution_account());
        let mut no_password = to_other_account_draft(&evolution_account());
        no_password.imap_source_uid = "other-acct".to_string();
        no_password.email = "robin@example.org".to_string();
        let mut no_smtp = to_other_account_draft(&evolution_account());
        no_smtp.smtp_host = String::new();
        let secrets = FakeEvolutionSecrets(HashMap::from([("acct-uid", "imap-secret"), ("transport-uid", "smtp-secret")]));
        let keyring = FakeKeyring::default();

        let imported = block_on(import_accounts(vec![complete, no_password, no_smtp], true, &secrets, &keyring));
        let stored: Vec<bool> = imported.iter().map(|i| i.passwords_stored).collect();
        assert_eq!(stored, [true, false, false]);
        let keyring = keyring.0.into_inner().unwrap();
        let first = &imported[0].account.id;
        assert_eq!(keyring[&(first.clone(), PROTOCOL_IMAP.to_string())], "imap-secret");
        assert_eq!(keyring[&(first.clone(), PROTOCOL_SMTP.to_string())], "smtp-secret");
        assert!(!imported[0].account.use_same_password);
        assert_eq!(keyring.len(), 2, "only the complete account's passwords are stored");
        assert!(imported.iter().all(|i| i.account.id.starts_with(OTHER_ACCOUNT_ID_PREFIX)));
    }

    #[test]
    fn importing_without_copying_passwords_never_reads_the_keyring() {
        let secrets = FakeEvolutionSecrets(HashMap::from([("acct-uid", "imap-secret")]));
        let keyring = FakeKeyring::default();
        let imported = block_on(import_accounts(vec![to_other_account_draft(&evolution_account())], false, &secrets, &keyring));
        assert!(!imported[0].passwords_stored);
        assert!(keyring.0.into_inner().unwrap().is_empty());
    }

    #[test]
    fn the_import_list_ticks_only_accounts_lookout_lacks() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let present = to_other_account_draft(&evolution_account());
        let mut missing = to_other_account_draft(&evolution_account());
        missing.email = "robin@example.org".to_string();
        missing.imap_username = "robin".to_string();
        let existing = [other_account("wren@example.org", "imap.example.org", "wren")];

        let (group, rows) = build_import_list(vec![present, missing], &existing, &[]);
        assert_eq!(rows.len(), 2);
        let ticked: Vec<bool> = rows.iter().map(|(check, _)| check.is_active()).collect();
        assert_eq!(ticked, [false, true]);
        assert!(!rows[0].0.is_sensitive(), "an account already present can't be ticked");

        // The group really holds two rows, the present one marked as such.
        let mut action_rows = Vec::new();
        let mut stack = vec![group.upcast::<gtk::Widget>()];
        while let Some(widget) = stack.pop() {
            if let Some(row) = widget.downcast_ref::<adw::ActionRow>() {
                action_rows.push(row.clone());
                continue;
            }
            let mut child = widget.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                stack.push(c);
            }
        }
        assert_eq!(action_rows.len(), 2);
        assert!(action_rows.iter().any(|row| row.subtitle().as_deref() == Some(ALREADY_PRESENT_SUBTITLE)));
        assert!(action_rows.iter().any(|row| row.title() == "Wren Example <robin@example.org>"));
    }

    /// Against the session's real EDS and keyring: lists the importable
    /// accounts and whether each password was found. Never prints a secret.
    /// Run with `cargo test -p lookout-app live_evolution_import -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_evolution_import() {
        block_on(async {
            for draft in evolution_account_drafts().await {
                let found = find_passwords(&EvolutionKeyring, &draft).await;
                println!(
                    "{} <{}>: IMAP {}:{} password {}, SMTP {}:{} password {}",
                    draft.display_name,
                    draft.email,
                    draft.imap_host,
                    draft.imap_port,
                    if found.imap.is_some() { "found" } else { "not found" },
                    draft.smtp_host,
                    draft.smtp_port,
                    if found.smtp.is_some() { "found" } else { "not found" },
                );
            }
        });
    }
}
