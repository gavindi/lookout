//! The People screen: CardDAV account discovery, per-account sync sessions
//! (RFC 6578 `sync-collection` deltas into each account's `ContactsCache`),
//! the left category tree and right contact list, and every contact dialog
//! (details/editor/import/export/groups). Extracted out of `window.rs` so
//! the contacts view has a real module home like `calendar_view` and
//! `config_view`. The mutable state it drives still lives on `UiState`
//! (the per-account snapshots, the `contact_cmd_tx` write channels, starred
//! favourites, and the deleted bucket); this module owns the sync loops and
//! the widgets built against them.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::{AccountId, ContactsProvider, EmailAddress, VCard};
use lookout_dav::{CardDavAccountConfig, ContactsCache, Credential, DavClient};
use lookout_goa::{ContactsAuthMethod, GoaClient, GoaContactsAccount};

use crate::window::UiState;
use crate::worker::Worker;

#[derive(Clone)]
pub struct ContactsAccountSnapshot {
    display_name: String,
    /// The address books discovered for this account - the write path's
    /// targets: creating a contact PUTs it under a book's href, and the
    /// contact editor's create mode offers the books as a picker.
    books: Vec<lookout_dav::AddressBookInfo>,
    /// Every vCard from every CardDAV address book discovered for this
    /// account, flattened - the People screen buckets these by fixed
    /// criteria (all/favourites/lists/deleted) and by vCard `CATEGORIES`
    /// rather than by which address book they came from (see
    /// `refresh_contacts_category_ui`), so which book a card came from
    /// doesn't need tracking here. Each record carries the server-side
    /// `href`/`getetag` the write path needs to PUT/DELETE it back.
    contacts: Vec<lookout_dav::ContactRecord>,
    suggestions: Vec<EmailAddress>,
}

/// Which bucket a left-pane row filters the right-hand contact list down to.
/// The first four are fixed, always-present rows under each account's
/// header; `Category` rows are dynamic, one per distinct vCard `CATEGORIES`
/// value found across every connected account (see `refresh_contacts_category_ui`).
#[derive(Clone)]
enum ContactsBucketKind {
    AllContacts,
    Favourites,
    ContactLists,
    Deleted,
    Category(String),
}

fn contacts_bucket_label(kind: &ContactsBucketKind) -> String {
    match kind {
        ContactsBucketKind::AllContacts => "Your Contacts".to_string(),
        ContactsBucketKind::Favourites => "Favourites".to_string(),
        ContactsBucketKind::ContactLists => "Your contact lists".to_string(),
        ContactsBucketKind::Deleted => "Deleted".to_string(),
        ContactsBucketKind::Category(name) => name.clone(),
    }
}

/// A `Category` bucket can span multiple accounts (the same tag used in two
/// address books), so contacts are tagged with their own account rather than
/// the choice carrying one account for all of them.
#[derive(Clone)]
pub struct ContactsCategoryChoice {
    kind: ContactsBucketKind,
    contacts: Vec<(AccountId, String, lookout_dav::ContactRecord)>,
}

#[derive(Clone)]
pub struct ContactsListEntry {
    pub account_id: AccountId,
    pub account_label: String,
    pub category_label: String,
    pub card: VCard,
    /// The server-side object metadata for `card`, needed by the editor to
    /// PUT/DELETE the contact back (`If-Match` guards against clobbering).
    /// `href` is empty for entries that only ever display a card (the
    /// in-memory Deleted bucket), where no write is possible.
    pub href: String,
    pub etag: Option<String>,
}

/// A write command for one account's CardDAV session, sent by the People
/// screen's editor/import/manage-groups flows and executed by the account's
/// poll loop in `sync_contacts_account`. Each command carries a one-shot
/// `reply` channel the UI awaits for the result (a toast or a dialog close),
/// so success/failure can't be conflated with the sync polling's `tx`.
pub enum ContactCommand {
    /// PUT a brand-new card under `book_href` as `<uid>.vcf` with
    /// `If-None-Match: *`.
    Create {
        book_href: String,
        card: VCard,
        reply: async_channel::Sender<Result<(), String>>,
    },
    /// PUT an edited card to its own `href` with `etag` as `If-Match`
    /// (a stale etag fails with HTTP 412 instead of clobbering).
    Update {
        href: String,
        etag: Option<String>,
        card: VCard,
        reply: async_channel::Sender<Result<(), String>>,
    },
    /// DELETE the card at `href` with `etag` as `If-Match`.
    Delete {
        href: String,
        etag: Option<String>,
        reply: async_channel::Sender<Result<(), String>>,
    },
    /// No-op write used purely to wake the poll loop (Config → "Clear all
    /// caches" sends one per account so the freshly-emptied caches rebuild
    /// right away instead of on the next poll tick).
    Refresh,
}

/// CardDAV has no push equivalent in this app yet, so contacts are refreshed
/// on a fixed polling interval.
const CONTACTS_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
fn contacts_match_prefix(contact: &EmailAddress, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let needle = prefix.to_lowercase();
    contact.address.to_lowercase().starts_with(&needle) || contact.name.as_deref().is_some_and(|name| name.to_lowercase().starts_with(&needle))
}

fn vcard_display_name(card: &VCard) -> String {
    if let Some(name) = card.full_name.as_deref().filter(|n| !n.trim().is_empty()) {
        return name.to_string();
    }
    if let Some(name) = &card.name {
        let mut parts = Vec::new();
        if !name.prefix.trim().is_empty() {
            parts.push(name.prefix.trim().to_string());
        }
        if !name.given.trim().is_empty() {
            parts.push(name.given.trim().to_string());
        }
        if !name.family.trim().is_empty() {
            parts.push(name.family.trim().to_string());
        }
        if !name.suffix.trim().is_empty() {
            parts.push(name.suffix.trim().to_string());
        }
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    card.emails.first().map(|email| email.address.clone()).unwrap_or_else(|| "(unnamed contact)".to_string())
}

fn vcard_primary_email(card: &VCard) -> String {
    card.emails.first().map(|email| email.address.clone()).unwrap_or_else(|| "No email address".to_string())
}

fn vcard_details_text(account_label: &str, category_label: &str, card: &VCard) -> String {
    let mut lines = vec![
        format!("Account: {account_label}"),
        format!("Category: {category_label}"),
        String::new(),
        format!("Name: {}", vcard_display_name(card)),
    ];

    if !card.emails.is_empty() {
        lines.push("Emails:".to_string());
        for email in &card.emails {
            let types = if email.types.is_empty() {
                "".to_string()
            } else {
                format!(" ({})", email.types.join(", "))
            };
            lines.push(format!("  - {}{}", email.address, types));
        }
    }

    if !card.telephones.is_empty() {
        lines.push("Phones:".to_string());
        for phone in &card.telephones {
            let types = if phone.types.is_empty() {
                "".to_string()
            } else {
                format!(" ({})", phone.types.join(", "))
            };
            lines.push(format!("  - {}{}", phone.number, types));
        }
    }

    if !card.addresses.is_empty() {
        lines.push("Addresses:".to_string());
        for address in &card.addresses {
            let mut parts = Vec::new();
            for part in [&address.street, &address.locality, &address.region, &address.postal_code, &address.country] {
                if !part.trim().is_empty() {
                    parts.push(part.trim().to_string());
                }
            }
            if !parts.is_empty() {
                lines.push(format!("  - {}", parts.join(", ")));
            }
        }
    }

    if let Some(org) = &card.organization {
        let value = org.iter().map(|part| part.trim()).filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" / ");
        if !value.is_empty() {
            lines.push(format!("Organization: {value}"));
        }
    }
    if let Some(title) = &card.title {
        if !title.trim().is_empty() {
            lines.push(format!("Title: {}", title.trim()));
        }
    }
    if let Some(note) = &card.note {
        if !note.trim().is_empty() {
            lines.push(String::new());
            lines.push("Notes:".to_string());
            lines.push(note.trim().to_string());
        }
    }

    lines.join("\n")
}

pub fn show_contact_details_dialog(window: &adw::ApplicationWindow, entry: &ContactsListEntry) {
    let dialog = gtk::Window::builder()
        .transient_for(window)
        .modal(true)
        .title(vcard_display_name(&entry.card))
        .default_width(520)
        .default_height(420)
        .build();

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();

    let details = gtk::Label::builder()
        .label(vcard_details_text(&entry.account_label, &entry.category_label, &entry.card))
        .xalign(0.0)
        .yalign(0.0)
        .selectable(true)
        .wrap(true)
        .build();
    details.set_margin_start(12);
    details.set_margin_end(12);
    details.set_margin_top(12);
    details.set_margin_bottom(12);

    let scroller = gtk::ScrolledWindow::builder().child(&details).vexpand(true).hexpand(true).build();

    let close_button = gtk::Button::with_label("Close");
    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| dialog.close());
    }
    close_button.set_halign(gtk::Align::End);
    close_button.set_margin_end(12);
    close_button.set_margin_bottom(12);

    content.append(&scroller);
    content.append(&close_button);
    dialog.set_child(Some(&content));
    dialog.present();
}

/// Sends one [`ContactCommand`] to an account's CardDAV session, returning
/// the one-shot reply channel the outcome arrives on. `None` when the
/// account has no session (or it's dead) - the caller decides how to
/// surface that (a toast, or a silent skip in a batched write).
fn dispatch_contact_command(
    state: &Rc<RefCell<UiState>>,
    account_id: &AccountId,
    build: impl FnOnce(async_channel::Sender<Result<(), String>>) -> ContactCommand,
) -> Option<async_channel::Receiver<Result<(), String>>> {
    let tx = state.borrow().contact_cmd_tx.get(account_id).cloned()?;
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    if tx.send_blocking(build(reply_tx)).is_err() {
        return None;
    }
    Some(reply_rx)
}

/// Sends one [`ContactCommand`] to an account's CardDAV session and toasts
/// its outcome. The command is built by `build` so the one-shot reply
/// channel pairs with the awaited result; a missing account or a dead
/// session answers with a toast instead of silently dropping the write.
fn send_contact_command(
    state: &Rc<RefCell<UiState>>,
    account_id: &AccountId,
    toast_overlay: &adw::ToastOverlay,
    success: &'static str,
    failure_prefix: &'static str,
    build: impl FnOnce(async_channel::Sender<Result<(), String>>) -> ContactCommand,
) {
    let Some(reply_rx) = dispatch_contact_command(state, account_id, build) else {
        toast_overlay.add_toast(adw::Toast::new("The contacts session isn't running."));
        return;
    };
    let toast_overlay = toast_overlay.clone();
    glib::spawn_future_local(async move {
        match reply_rx.recv().await {
            Ok(Ok(())) => toast_overlay.add_toast(adw::Toast::new(success)),
            Ok(Err(message)) => {
                let text = glib::markup_escape_text(&format!("{failure_prefix}: {message}"));
                toast_overlay.add_toast(adw::Toast::new(&text));
            }
            // The session died mid-write; its error handling owns the toast.
            Err(_) => {}
        }
    });
}

/// Routes a created contact to its account's session: a `Create` command
/// PUTs `<uid>.vcf` under `book_href` with `If-None-Match: *`.
fn route_contact_create(state: &Rc<RefCell<UiState>>, account_id: AccountId, book_href: String, card: VCard, toast_overlay: &adw::ToastOverlay) {
    if book_href.is_empty() {
        toast_overlay.add_toast(adw::Toast::new("Choose an address book to create the contact in."));
        return;
    }
    send_contact_command(state, &account_id, toast_overlay, "Contact created", "Couldn't create contact", move |reply| {
        ContactCommand::Create { book_href, card, reply }
    });
}

/// Routes an edited contact to its account's session: an `Update` command
/// PUTs it to its own href with its etag as `If-Match`.
fn route_contact_update(state: &Rc<RefCell<UiState>>, account_id: AccountId, href: String, etag: Option<String>, card: VCard, toast_overlay: &adw::ToastOverlay) {
    send_contact_command(state, &account_id, toast_overlay, "Contact saved", "Couldn't save contact", move |reply| {
        ContactCommand::Update { href, etag, card, reply }
    });
}

/// Routes a deleted contact to its account's session (`Delete` with the
/// etag as `If-Match`). The session's post-write resync lands the card in
/// the Deleted bucket via the usual diff.
fn route_contact_delete(state: &Rc<RefCell<UiState>>, account_id: AccountId, href: String, etag: Option<String>, toast_overlay: &adw::ToastOverlay) {
    send_contact_command(state, &account_id, toast_overlay, "Contact deleted", "Couldn't delete contact", move |reply| {
        ContactCommand::Delete { href, etag, reply }
    });
}

/// Opens the contact editor for an existing, writable contact - the write
/// target (account, address books, href/etag) is taken from the entry's
/// account snapshot.
pub fn show_contact_editor_for(window: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, toast_overlay: &adw::ToastOverlay, entry: &ContactsListEntry) {
    let account_id = entry.account_id.clone();
    let Some(snapshot) = state.borrow().contacts_by_account.get(&account_id).cloned() else {
        toast_overlay.add_toast(adw::Toast::new("Contact account isn't available."));
        return;
    };
    let books: Vec<(String, String)> = snapshot.books.iter().map(|book| (book.display_name.clone(), book.href.clone())).collect();
    let state = state.clone();
    let toast_overlay = toast_overlay.clone();
    crate::contacts_editor::show_contact_editor(
        window,
        crate::contacts_editor::ContactEditorPrefill {
            books: &books,
            existing: Some(&entry.card),
            href: &entry.href,
            etag: entry.etag.as_deref(),
        },
        // A create callback for an edit is unreachable; the dialog only
        // fires it on the blank form.
        move |_book_href, _card| {},
        {
            let state = state.clone();
            let account_id = account_id.clone();
            let toast_overlay = toast_overlay.clone();
            move |href, etag, card| route_contact_update(&state, account_id.clone(), href, etag, card, &toast_overlay)
        },
        {
            let state = state.clone();
            let account_id = account_id.clone();
            let toast_overlay = toast_overlay.clone();
            move |href, etag| route_contact_delete(&state, account_id.clone(), href, etag, &toast_overlay)
        },
    );
}

/// Opens the blank "new contact" editor, routed to the first account that
/// has a writable address book (the picker inside the dialog lists that
/// account's books; the People screen's buckets are per-account, so a
/// global New Contact button has to pick a home up front).
pub fn show_new_contact_editor(window: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, toast_overlay: &adw::ToastOverlay) {
    let st = state.borrow();
    let mut accounts: Vec<(&AccountId, &ContactsAccountSnapshot)> = st.contacts_by_account.iter().collect();
    accounts.sort_by_key(|(_, snapshot)| snapshot.display_name.to_lowercase());
    let Some((account_id, snapshot)) = accounts.into_iter().find(|(_, snapshot)| !snapshot.books.is_empty()) else {
        drop(st);
        toast_overlay.add_toast(adw::Toast::new("Connect a contacts account to create contacts."));
        return;
    };
    let account_id = account_id.clone();
    let books: Vec<(String, String)> = snapshot.books.iter().map(|book| (book.display_name.clone(), book.href.clone())).collect();
    drop(st);
    let state = state.clone();
    let toast_overlay = toast_overlay.clone();
    crate::contacts_editor::show_contact_editor(
        window,
        crate::contacts_editor::ContactEditorPrefill {
            books: &books,
            existing: None,
            href: "",
            etag: None,
        },
        {
            let state = state.clone();
            let account_id = account_id.clone();
            let toast_overlay = toast_overlay.clone();
            move |book_href, card| route_contact_create(&state, account_id.clone(), book_href, card, &toast_overlay)
        },
        // Unreachable on the blank form; update/delete only fire for an edit.
        move |_href, _etag, _card| {},
        move |_href, _etag| {},
    );
}

/// "Import…" for the People screen: pick a `.vcf` file, then route every
/// card in it to a chosen address book. Deduplication is per the accepted
/// policy: a card whose `UID` matches an existing contact in the target
/// *account* updates that contact in place (its server href/etag are kept,
/// so the write is guarded like an edit); a card without a matching UID but
/// with an email the account already has is skipped and counted; anything
/// else is created. One summary toast reports the three counts when every
/// write has answered.
pub fn show_contacts_import_dialog(window: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, toast_overlay: &adw::ToastOverlay) {
    // Target books: every writable address book across every connected
    // account, labelled "account · book" like the event editor's calendar
    // picker (book names alone repeat across accounts).
    let mut books: Vec<(AccountId, String, String)> = Vec::new();
    {
        let st = state.borrow();
        let mut accounts: Vec<(&AccountId, &ContactsAccountSnapshot)> = st.contacts_by_account.iter().collect();
        accounts.sort_by_key(|(_, snapshot)| snapshot.display_name.to_lowercase());
        for (account_id, snapshot) in accounts {
            for book in &snapshot.books {
                books.push((account_id.clone(), book.href.clone(), format!("{} · {}", snapshot.display_name, book.display_name)));
            }
        }
    }
    if books.is_empty() {
        toast_overlay.add_toast(adw::Toast::new("Connect a contacts account to import into."));
        return;
    }

    let dialog = gtk::Window::builder().transient_for(window).modal(true).title("Import contacts").default_width(480).build();

    let choose_button = gtk::Button::with_label("Choose file…");
    let file_label = gtk::Label::builder().label("No file chosen").xalign(0.0).css_classes(["dim-label"]).build();
    let chosen_vcf: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let file_group = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    file_group.append(&choose_button);
    file_group.append(&file_label);

    let labels: Vec<String> = books.iter().map(|(_, _, label)| label.clone()).collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let string_list = gtk::StringList::new(&label_refs);
    let book_dropdown = gtk::DropDown::builder().model(&string_list).build();
    let target_group = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    let target_label = gtk::Label::builder().label("Import into").xalign(0.0).build();
    target_group.append(&target_label);
    target_group.append(&book_dropdown);

    let import_button = gtk::Button::with_label("Import");
    import_button.add_css_class("suggested-action");
    import_button.set_sensitive(false);
    let close_button = gtk::Button::with_label("Close");

    {
        let window = window.clone();
        let chosen_vcf = chosen_vcf.clone();
        let file_label = file_label.clone();
        let import_button = import_button.clone();
        choose_button.connect_clicked(move |_| {
            let window = window.clone();
            let chosen_vcf = chosen_vcf.clone();
            let file_label = file_label.clone();
            let import_button = import_button.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_suffix("vcf");
                filter.add_suffix("vcard");
                filter.set_name(Some("vCard files (*.vcf, *.vcard)"));
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let picker = gtk::FileDialog::builder().title("Choose a vCard file").filters(&filters).build();
                let Ok(file) = picker.open_future(Some(&window)).await else { return };
                let Some(path) = file.path() else { return };
                let Ok(raw) = std::fs::read(&path) else { return };
                let text = String::from_utf8_lossy(&raw).into_owned();
                if lookout_core::VCard::parse_all(&text).is_empty() {
                    file_label.set_label("No vCards found in this file.");
                    return;
                }
                *chosen_vcf.borrow_mut() = Some(text);
                file_label.set_label(&path.display().to_string());
                import_button.set_sensitive(true);
            });
        });
    }

    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| dialog.close());
    }

    {
        let dialog = dialog.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        let chosen_vcf = chosen_vcf.clone();
        let books = books.clone();
        let book_dropdown = book_dropdown.clone();
        import_button.connect_clicked(move |_| {
            let Some(text) = chosen_vcf.borrow().clone() else { return };
            let Some((account_id, book_href, _)) = books.get(book_dropdown.selected() as usize).cloned() else {
                return;
            };

            // Existing UIDs and emails of the target *account*: the
            // dedupe keys. Owned copies - the state borrow must end before
            // the dispatch loop below borrows it again.
            let existing_by_uid: HashMap<String, (String, Option<String>)> = {
                let st = state.borrow();
                st.contacts_by_account
                    .get(&account_id)
                    .into_iter()
                    .flat_map(|snapshot| &snapshot.contacts)
                    .filter_map(|record| record.card.uid.as_deref().map(|uid| (uid.to_string(), (record.href.clone(), record.etag.clone()))))
                    .collect()
            };
            let existing_emails: HashSet<String> = {
                let st = state.borrow();
                st.contacts_by_account
                    .get(&account_id)
                    .into_iter()
                    .flat_map(|snapshot| &snapshot.contacts)
                    .flat_map(|record| record.card.emails.iter())
                    .map(|email| email.address.trim().to_lowercase())
                    .filter(|key| !key.is_empty())
                    .collect()
            };

            let mut created = 0usize;
            let mut updated = 0usize;
            let mut skipped = 0usize;
            let mut replies = Vec::new();
            for result in lookout_core::VCard::parse_all(&text) {
                let Ok(mut card) = result else {
                    skipped += 1;
                    continue;
                };
                let uid = card.uid.clone().unwrap_or_default();
                if let Some((href, etag)) = existing_by_uid.get(&uid) {
                    // Same UID: update in place, keeping the server metadata
                    // so the write is an If-Match-guarded edit.
                    let href = href.clone();
                    let etag = etag.clone();
                    if let Some(rx) = dispatch_contact_command(&state, &account_id, move |reply| ContactCommand::Update { href, etag, card, reply }) {
                        replies.push(rx);
                    }
                    updated += 1;
                } else if card.emails.iter().any(|email| existing_emails.contains(&email.address.trim().to_lowercase())) {
                    skipped += 1;
                } else {
                    // A file card without a UID gets one before the PUT so
                    // two such cards can't collide on the same `<uid>.vcf`
                    // href.
                    if card.uid.is_none() {
                        card.uid = Some(uuid::Uuid::new_v4().to_string());
                    }
                    let book_href = book_href.clone();
                    if let Some(rx) = dispatch_contact_command(&state, &account_id, move |reply| ContactCommand::Create { book_href, card, reply }) {
                        replies.push(rx);
                    }
                    created += 1;
                }
            }

            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                let mut failures = 0;
                for reply in replies {
                    if !matches!(reply.recv().await, Ok(Ok(()))) {
                        failures += 1;
                    }
                }
                let summary = match (created, updated, skipped) {
                    (0, 0, _) => format!("{skipped} contact(s) already exist - nothing imported"),
                    _ => format!("Imported {created}, updated {updated}, skipped {skipped} duplicate(s)"),
                };
                if failures > 0 {
                    toast_overlay.add_toast(adw::Toast::new(&format!("{summary} ({failures} failed)")));
                } else {
                    toast_overlay.add_toast(adw::Toast::new(&summary));
                }
            });
            dialog.close();
        });
    }

    let button_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    button_row.set_halign(gtk::Align::End);
    button_row.append(&close_button);
    button_row.append(&import_button);

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    content.append(&file_group);
    content.append(&target_group);
    content.append(&button_row);
    dialog.set_child(Some(&content));
    dialog.present();
}

/// "Export…" for the People screen: saves every contact in the currently
/// selected bucket as one multi-card `.vcf` file (the same file shape
/// `parse_all` imports).
pub fn export_current_contacts(window: &adw::ApplicationWindow, entries: &Rc<RefCell<Vec<ContactsListEntry>>>, toast_overlay: &adw::ToastOverlay) {
    let cards: Vec<VCard> = entries.borrow().iter().map(|entry| entry.card.clone()).collect();
    if cards.is_empty() {
        toast_overlay.add_toast(adw::Toast::new("Nothing to export in this category."));
        return;
    }
    let window = window.clone();
    let toast_overlay = toast_overlay.clone();
    glib::spawn_future_local(async move {
        let filter = gtk::FileFilter::new();
        filter.add_suffix("vcf");
        filter.set_name(Some("vCard files (*.vcf)"));
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Export contacts").filters(&filters).initial_name("contacts.vcf").build();
        let Ok(file) = dialog.save_future(Some(&window)).await else { return };
        let Some(path) = file.path() else { return };
        let mut body = String::new();
        for card in &cards {
            body.push_str(&card.serialize());
            body.push_str("\r\n");
        }
        match std::fs::write(&path, body) {
            Ok(()) => toast_overlay.add_toast(adw::Toast::new(&format!("Exported {} contact(s)", cards.len()))),
            Err(e) => toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't export contacts: {e}")))),
        }
    });
}

fn dedupe_addresses(addresses: Vec<EmailAddress>, limit: usize) -> Vec<EmailAddress> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for addr in addresses {
        let key = addr.address.trim().to_lowercase();
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        out.push(addr);
        if out.len() >= limit {
            break;
        }
    }
    out
}

pub fn merge_contact_suggestions(mail_history: Vec<EmailAddress>, carddav: &[EmailAddress], prefix: &str, limit: usize) -> Vec<EmailAddress> {
    let mut merged = Vec::new();
    merged.extend(mail_history);
    merged.extend(carddav.iter().filter(|c| contacts_match_prefix(c, prefix)).cloned());
    dedupe_addresses(merged, limit)
}

/// The shared `ContactsProvider` implementation: a live view over the
/// in-memory CardDAV contact snapshots (`UiState::contacts_by_account`).
/// `account: Some(id)` scopes the search to one account (the composer, which
/// completes from the account being sent from); `None` unions every connected
/// account (the calendar attendees, where an invitee isn't tied to the
/// event's calendar account). The trait deliberately lives in `lookout-core`
/// so the mail cache (`lookout-mail`'s history-based impl) and the CardDAV
/// app state share one autocomplete interface.
pub struct SnapshotContactsProvider {
    pub state: Rc<RefCell<UiState>>,
    pub account: Option<AccountId>,
}

impl ContactsProvider for SnapshotContactsProvider {
    fn search_contacts(&self, prefix: &str, limit: usize) -> Vec<EmailAddress> {
        let prefix = prefix.trim();
        let st = self.state.borrow();
        let candidates: Vec<EmailAddress> = match &self.account {
            Some(id) => st.contacts_by_account.get(id).into_iter().flat_map(|snapshot| snapshot.suggestions.clone()).collect(),
            None => st.contacts_by_account.values().flat_map(|snapshot| snapshot.suggestions.clone()).collect(),
        };
        drop(st);
        let filtered = candidates.into_iter().filter(|c| contacts_match_prefix(c, prefix));
        dedupe_addresses(filtered.collect(), limit)
    }
}

/// Attendee autocomplete for the event editor's "Invite required attendees"
/// field. Unlike the composer's per-account `suggestions` (`build_composer`'s
/// caller scopes it to the account being sent from), an invitee isn't tied
/// to any one account - so this merges mail-history hits and CardDAV
/// suggestions across *every* connected account (same source data as
/// `search_cached_results`/the composer's own suggestion source, just
/// unioned instead of scoped).
pub fn calendar_attendee_suggestions(state: &Rc<RefCell<UiState>>) -> crate::recipient_entry::SuggestionSource {
    let state = state.clone();
    let carddav_provider = SnapshotContactsProvider {
        state: state.clone(),
        account: None,
    };
    Rc::new(move |prefix: &str| {
        let prefix = prefix.trim();
        let st = state.borrow();
        let mail_history: Vec<EmailAddress> = st
            .accounts
            .values()
            .filter_map(|h| h.address_cache.as_ref())
            .flat_map(|cache| cache.search_contacts(prefix, 8))
            .collect();
        drop(st);
        merge_contact_suggestions(mail_history, &carddav_provider.search_contacts(prefix, 8), prefix, 8)
    })
}

/// A stable-ish key for a contact, used for UI state (`starred_contacts`,
/// deletion diffing) that has nowhere else to hang off of: the vCard's
/// `UID`, or its first email address if the server didn't send one.
fn contact_identity(card: &VCard) -> String {
    card.uid.clone().unwrap_or_else(|| vcard_primary_email(card))
}

/// Appends a non-selectable section-header row (an account's display name,
/// or "Categories") to the People screen's left pane.
fn append_contacts_header_row(list: &gtk::ListBox, label: &str) -> gtk::ListBoxRow {
    let row_label = gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .css_classes(["heading"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    row_label.set_margin_start(8);
    row_label.set_margin_end(8);
    row_label.set_margin_top(10);
    row_label.set_margin_bottom(4);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_label));
    row.set_selectable(false);
    row.set_activatable(false);
    list.append(&row);
    row
}

/// Appends a selectable bucket/category row, indented under its section
/// header, to the People screen's left pane.
fn append_contacts_choice_row(list: &gtk::ListBox, label: &str) -> gtk::ListBoxRow {
    let row_label = gtk::Label::builder().label(label).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    row_label.set_margin_start(20);
    row_label.set_margin_end(8);
    row_label.set_margin_top(6);
    row_label.set_margin_bottom(6);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_label));
    list.append(&row);
    row
}

/// Rebuilds the People screen's left pane: each connected account gets a
/// header followed by its four fixed buckets (Your Contacts/Favourites/Your
/// contact lists/Deleted), then every distinct vCard `CATEGORIES` tag found
/// across all accounts gets its own row under a trailing "Categories"
/// header. Header rows aren't part of `categories` and can't be selected;
/// each selectable row stashes its index into `categories` on itself (see
/// `contacts_category_list.connect_row_selected` in `build`), since a row's
/// position in the widget no longer matches its index in that list once
/// headers are mixed in.
pub fn refresh_contacts_category_ui(
    state: &Rc<RefCell<UiState>>,
    category_list: &gtk::ListBox,
    contact_list: &gtk::ListBox,
    categories: &Rc<RefCell<Vec<ContactsCategoryChoice>>>,
    contacts: &Rc<RefCell<Vec<ContactsListEntry>>>,
    selected_index: Option<i32>,
) {
    while let Some(child) = category_list.first_child() {
        category_list.remove(&child);
    }

    let mut rows: Vec<ContactsCategoryChoice> = Vec::new();
    // Parallel to `rows`: the widget for each row appended so far, alongside
    // the `rows` index it selects (`None` for header rows).
    let mut row_widgets: Vec<(gtk::ListBoxRow, Option<usize>)> = Vec::new();

    {
        let st = state.borrow();
        let mut accounts: Vec<(&AccountId, &ContactsAccountSnapshot)> = st.contacts_by_account.iter().collect();
        accounts.sort_by_key(|(_, data)| data.display_name.to_lowercase());

        // Every distinct CATEGORIES tag across every account, so the same
        // tag used by contacts in two different address books still gets
        // one combined row rather than one per account.
        let mut category_members: HashMap<String, Vec<(AccountId, String, lookout_dav::ContactRecord)>> = HashMap::new();

        for (account_id, account) in accounts {
            let all_contacts: Vec<(AccountId, String, lookout_dav::ContactRecord)> = account
                .contacts
                .iter()
                .map(|record| (account_id.clone(), account.display_name.clone(), record.clone()))
                .collect();

            for (_, _, record) in &all_contacts {
                for tag in &record.card.categories {
                    category_members
                        .entry(tag.clone())
                        .or_default()
                        .push((account_id.clone(), account.display_name.clone(), record.clone()));
                }
            }

            // vCard v4's KIND:group cards represent a named contact list
            // rather than a person.
            let contact_lists: Vec<(AccountId, String, lookout_dav::ContactRecord)> =
                all_contacts.iter().filter(|(_, _, record)| record.card.kind.as_deref() == Some("group")).cloned().collect();
            let deleted: Vec<(AccountId, String, lookout_dav::ContactRecord)> = st
                .deleted_contacts
                .get(account_id)
                .into_iter()
                .flatten()
                .map(|card| {
                    (
                        account_id.clone(),
                        account.display_name.clone(),
                        lookout_dav::ContactRecord {
                            // Deleted-bucket cards are display-only (the diff
                            // against a poll only kept the card, not its
                            // server metadata): an empty href marks them as
                            // not writable, and the editor's Delete is
                            // disabled for them.
                            href: String::new(),
                            etag: None,
                            card: card.clone(),
                        },
                    )
                })
                .collect();

            row_widgets.push((append_contacts_header_row(category_list, &account.display_name), None));

            // Favourites keeps the full, unfiltered contact list - which
            // contacts belong depends on `starred_contacts` (persisted in
            // `ui_state_db`) state that can change without a resync, so the
            // filter is applied live in `rebuild_contacts_list_ui` instead
            // of baked in here.
            for kind in [ContactsBucketKind::AllContacts, ContactsBucketKind::Favourites] {
                rows.push(ContactsCategoryChoice {
                    kind: kind.clone(),
                    contacts: all_contacts.clone(),
                });
                let label = contacts_bucket_label(&kind);
                row_widgets.push((append_contacts_choice_row(category_list, &label), Some(rows.len() - 1)));
            }

            rows.push(ContactsCategoryChoice {
                kind: ContactsBucketKind::ContactLists,
                contacts: contact_lists,
            });
            row_widgets.push((
                append_contacts_choice_row(category_list, &contacts_bucket_label(&ContactsBucketKind::ContactLists)),
                Some(rows.len() - 1),
            ));

            rows.push(ContactsCategoryChoice {
                kind: ContactsBucketKind::Deleted,
                contacts: deleted,
            });
            row_widgets.push((
                append_contacts_choice_row(category_list, &contacts_bucket_label(&ContactsBucketKind::Deleted)),
                Some(rows.len() - 1),
            ));
        }

        if !category_members.is_empty() {
            row_widgets.push((append_contacts_header_row(category_list, "Categories"), None));
            let mut names: Vec<String> = category_members.keys().cloned().collect();
            names.sort_by_key(|name| name.to_lowercase());
            for name in names {
                let members = category_members.remove(&name).unwrap_or_default();
                rows.push(ContactsCategoryChoice {
                    kind: ContactsBucketKind::Category(name.clone()),
                    contacts: members,
                });
                row_widgets.push((append_contacts_choice_row(category_list, &name), Some(rows.len() - 1)));
            }
        }
    }

    *categories.borrow_mut() = rows;
    for (row, index) in &row_widgets {
        if let Some(idx) = index {
            unsafe { row.set_data("contacts-choice-index", *idx) };
        }
    }

    if categories.borrow().is_empty() {
        while let Some(child) = contact_list.first_child() {
            contact_list.remove(&child);
        }
        contacts.borrow_mut().clear();
        let label = gtk::Label::builder().label("No contacts available yet").xalign(0.0).css_classes(["dim-label"]).build();
        label.set_margin_start(10);
        label.set_margin_end(10);
        label.set_margin_top(10);
        label.set_margin_bottom(10);
        contact_list.append(&label);
        return;
    }

    let desired = selected_index.unwrap_or(0).clamp(0, categories.borrow().len() as i32 - 1);
    if let Some((row, _)) = row_widgets.iter().find(|(_, idx)| *idx == Some(desired as usize)) {
        category_list.select_row(Some(row));
    }
    rebuild_contacts_list_ui(contact_list, categories, contacts, desired, state);
}

/// Rebuilds the People screen's right-hand contact list for whichever
/// bucket is selected in the left pane. Takes `state` so each row can show
/// (and toggle) its favourite star live - Favourites membership is
/// evaluated here rather than pre-filtered in `refresh_contacts_category_ui`
/// so starring/unstarring a contact doesn't require rebuilding the whole
/// left pane, just this list.
pub fn rebuild_contacts_list_ui(
    contact_list: &gtk::ListBox,
    categories: &Rc<RefCell<Vec<ContactsCategoryChoice>>>,
    contacts: &Rc<RefCell<Vec<ContactsListEntry>>>,
    selected_category_index: i32,
    state: &Rc<RefCell<UiState>>,
) {
    while let Some(child) = contact_list.first_child() {
        contact_list.remove(&child);
    }

    let Some(choice) = categories.borrow().get(selected_category_index as usize).cloned() else {
        contacts.borrow_mut().clear();
        return;
    };

    let category_label = contacts_bucket_label(&choice.kind);
    let favourites_only = matches!(choice.kind, ContactsBucketKind::Favourites);

    let mut entries: Vec<ContactsListEntry> = {
        let st = state.borrow();
        choice
            .contacts
            .into_iter()
            .filter(|(account_id, _, record)| !favourites_only || st.starred_contacts.contains(&(account_id.clone(), contact_identity(&record.card))))
            .map(|(account_id, account_label, record)| ContactsListEntry {
                account_id,
                account_label,
                category_label: category_label.clone(),
                card: record.card,
                href: record.href,
                etag: record.etag,
            })
            .collect()
    };
    entries.sort_by_key(|entry| vcard_display_name(&entry.card).to_lowercase());

    if entries.is_empty() {
        let label = gtk::Label::builder().label("No contacts in this category").xalign(0.0).css_classes(["dim-label"]).build();
        label.set_margin_start(10);
        label.set_margin_end(10);
        label.set_margin_top(10);
        label.set_margin_bottom(10);
        contact_list.append(&label);
        contacts.borrow_mut().clear();
        return;
    }

    for entry in &entries {
        let name = gtk::Label::builder()
            .label(vcard_display_name(&entry.card))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let email = gtk::Label::builder()
            .label(vcard_primary_email(&entry.card))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let text_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        text_box.append(&name);
        text_box.append(&email);

        let identity = contact_identity(&entry.card);
        let is_starred = state.borrow().starred_contacts.contains(&(entry.account_id.clone(), identity.clone()));
        let star_button = gtk::ToggleButton::builder()
            .icon_name("starred-symbolic")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .active(is_starred)
            .tooltip_text("Favourite")
            .build();
        {
            let state = state.clone();
            let contact_list = contact_list.clone();
            let categories = categories.clone();
            let contacts = contacts.clone();
            let account_id = entry.account_id.clone();
            star_button.connect_toggled(move |btn| {
                let key = (account_id.clone(), identity.clone());
                if btn.is_active() {
                    state.borrow_mut().starred_contacts.insert(key);
                } else {
                    state.borrow_mut().starred_contacts.remove(&key);
                }
                // Phase 5: persist the star in the UI-state database so
                // favourites survive restarts; a failing database is already
                // logged at open time and only costs persistence.
                if let Some(db) = state.borrow().ui_db.clone() {
                    let _ = db.set_starred(&account_id, &identity, btn.is_active());
                }
                rebuild_contacts_list_ui(&contact_list, &categories, &contacts, selected_category_index, &state);
            });
        }

        let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
        row_box.set_margin_start(10);
        row_box.set_margin_end(10);
        row_box.set_margin_top(6);
        row_box.set_margin_bottom(6);
        row_box.append(&text_box);
        row_box.append(&star_button);
        contact_list.append(&row_box);
    }
    *contacts.borrow_mut() = entries;
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_contacts_discovery(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    root_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    current_contacts_page: Rc<Cell<&'static str>>,
    contacts_view_button: gtk::ToggleButton,
    refresh_contacts_ui: Rc<dyn Fn(Option<i32>)>,
) {
    let (goa_tx, goa_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let result = async {
            let client = GoaClient::connect().await?;
            let accounts = client.list_contacts_accounts().await?;
            Ok::<_, lookout_goa::Error>((client, accounts))
        }
        .await;
        let _ = goa_tx.send(result).await;
    });

    glib::spawn_future_local(async move {
        let Ok(result) = goa_rx.recv().await else { return };
        let show_page = |name: &'static str| {
            current_contacts_page.set(name);
            if contacts_view_button.is_active() {
                root_stack.set_visible_child_name(name);
            }
        };
        match result {
            Ok((client, accounts)) if !accounts.is_empty() => {
                show_page("contacts");
                for account in accounts {
                    // Each account's session gets its own command channel,
                    // stored so the People screen's editor/import/group flows
                    // can route writes to the right account.
                    let (cmd_tx, cmd_rx) = async_channel::unbounded();
                    state.borrow_mut().contact_cmd_tx.insert(account.account_id.clone(), cmd_tx);
                    sync_contacts_account(
                        worker.clone(),
                        state.clone(),
                        toast_overlay.clone(),
                        client.clone(),
                        account,
                        cmd_rx,
                        refresh_contacts_ui.clone(),
                    );
                }
            }
            Ok(_) => show_page("contacts-empty"),
            Err(e) => {
                show_page("contacts-empty");
                toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't reach GNOME Online Accounts contacts: {e}"))));
            }
        }
    });
}

pub fn sync_contacts_account(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    toast_overlay: adw::ToastOverlay,
    goa_client: GoaClient,
    account: GoaContactsAccount,
    cmd_rx: async_channel::Receiver<ContactCommand>,
    refresh_contacts_ui: Rc<dyn Fn(Option<i32>)>,
) {
    let account_id = account.account_id.clone();
    let label = account.display_name.clone();
    let (tx, rx) = async_channel::unbounded();
    let cache_account_id = account_id.clone();
    worker.spawn(async move {
        // The per-account CardDAV cache: fast-paint source at startup, the
        // full contact set deltas are applied to, and the RFC 6578 sync-token
        // store. Best-effort - a failed open just means no cache (full
        // refetches every poll, no fast paint), never an error.
        let cache = ContactsCache::open(&cache_account_id).ok();
        let mut first = true;
        // Render whatever the previous session cached immediately, so the
        // People screen shows data before the first live poll completes.
        if let Some(cache) = cache.as_ref() {
            if let Some(snapshot) = snapshot_from_cache(&account, cache) {
                if tx.send((first, Ok(snapshot))).await.is_err() {
                    return;
                }
                first = false;
            }
        }
        loop {
            // Interleave the poll cadence with write commands: a poll tick
            // wakes a fetch, a command is executed with a fresh credential
            // right away (the same pattern as `run_calendar_session`'s
            // select), and a successful write forces an immediate resync so
            // the change renders before the next poll.
            let wake = if first {
                None
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(CONTACTS_REFRESH_INTERVAL) => None,
                    cmd = cmd_rx.recv() => match cmd {
                        Ok(cmd) => Some(cmd),
                        Err(_) => break,
                    },
                }
            };

            let mut resync = wake.is_none();
            // Process the command that woke us (if any), then drain any
            // further commands queued while we were mid-fetch.
            for command in wake.into_iter().chain(std::iter::from_fn(|| cmd_rx.try_recv().ok())) {
                match command {
                    ContactCommand::Create { book_href, card, reply } => {
                        let result = put_contact(goa_client.clone(), account.clone(), new_contact_href(&book_href, &card), &card, None).await;
                        let _ = reply.send(result.clone()).await;
                        if result.is_ok() {
                            resync = true;
                        }
                    }
                    ContactCommand::Update { href, etag, card, reply } => {
                        let result = put_contact(goa_client.clone(), account.clone(), href, &card, etag).await;
                        let _ = reply.send(result.clone()).await;
                        if result.is_ok() {
                            resync = true;
                        }
                    }
                    ContactCommand::Delete { href, etag, reply } => {
                        let result = delete_contact(goa_client.clone(), account.clone(), href, etag).await;
                        let _ = reply.send(result.clone()).await;
                        if result.is_ok() {
                            resync = true;
                        }
                    }
                    ContactCommand::Refresh => {
                        resync = true;
                    }
                }
            }

            if !resync {
                continue;
            }
            let result = fetch_carddav_contacts(goa_client.clone(), account.clone(), cache.as_ref()).await;
            if tx.send((first, result)).await.is_err() {
                break;
            }
            first = false;
        }
    });

    glib::spawn_future_local(async move {
        while let Ok((is_first, result)) = rx.recv().await {
            match result {
                Ok(contacts) => {
                    let mut st = state.borrow_mut();
                    // The snapshots the loop sends are the *complete* contact
                    // set for the account (rebuilt from the cache after the
                    // per-book deltas are applied), so a contact missing from
                    // the new set really was deleted upstream - either a
                    // delta's 404 href, a full-refetch diff, or a book that
                    // vanished. Diff against what the previous poll saw and
                    // stash anything missing in `deleted_contacts` before
                    // it's overwritten below. A write's post-command resync
                    // lands here too - which is how a locally-deleted contact
                    // reaches this bucket without waiting for the next poll.
                    if let Some(previous) = st.contacts_by_account.get(&account_id) {
                        let previously_seen: HashMap<String, VCard> = previous.contacts.iter().map(|record| (contact_identity(&record.card), record.card.clone())).collect();
                        let still_present: HashSet<String> = contacts.contacts.iter().map(|record| contact_identity(&record.card)).collect();
                        let deleted_bucket = st.deleted_contacts.entry(account_id.clone()).or_default();
                        let already_tracked: HashSet<String> = deleted_bucket.iter().map(contact_identity).collect();
                        for (identity, card) in previously_seen {
                            if !still_present.contains(&identity) && !already_tracked.contains(&identity) {
                                deleted_bucket.push(card);
                            }
                        }
                    }
                    st.contacts_by_account.insert(account_id.clone(), contacts);
                    drop(st);
                    refresh_contacts_ui(None);
                }
                Err(message) => {
                    tracing::warn!("CardDAV contact sync failed for {label}: {message}");
                    // Surface an initial failure to the user, but avoid
                    // repeating the same toast every poll interval.
                    if is_first {
                        let text = glib::markup_escape_text(&format!("{label}: {message}"));
                        toast_overlay.add_toast(adw::Toast::new(&text));
                    }
                }
            }
        }
    });
}

/// Builds the authenticated DAV client + fresh credential for one CardDAV
/// account - shared by the poll fetch and the write path, so a command
/// executed between polls authenticates exactly like a poll would.
async fn carddav_connection(goa_client: &GoaClient, account: &GoaContactsAccount) -> Result<(DavClient, Credential), String> {
    goa_client.ensure_credentials_contacts(account).await.map_err(|e| e.to_string())?;
    let credential = match &account.auth {
        ContactsAuthMethod::OAuth2 => {
            let (token, _expires_in) = goa_client.get_access_token_contacts(account).await.map_err(|e| e.to_string())?;
            Credential::OAuth2AccessToken(token)
        }
        ContactsAuthMethod::Password { .. } => {
            let password = goa_client.get_contacts_password(account).await.map_err(|e| e.to_string())?;
            Credential::Password(password)
        }
    };

    let config = CardDavAccountConfig {
        account_id: account.account_id.clone(),
        display_name: account.display_name.clone(),
        base_url: account.uri.clone(),
        accept_ssl_errors: account.accept_ssl_errors,
        // `Account.Identity` is the login username the DAV server expects
        // (e.g. the Nextcloud user id); the display name is only a fallback
        // for providers that don't advertise an Identity.
        username: if account.identity.is_empty() {
            account.display_name.clone()
        } else {
            account.identity.clone()
        },
    };
    let client = DavClient::new(&config.base_url, config.accept_ssl_errors, config.username.clone()).map_err(|e| e.to_string())?;
    Ok((client, credential))
}

/// The PUT half of a contact write: `etag` guards an update (`If-Match`), so
/// an edit based on a stale copy fails with HTTP 412 instead of clobbering a
/// concurrent change; `None` marks a create (`If-None-Match: *`).
async fn put_contact(goa_client: GoaClient, account: GoaContactsAccount, href: String, card: &VCard, etag: Option<String>) -> Result<(), String> {
    let (client, credential) = carddav_connection(&goa_client, &account).await?;
    client
        .put_contact_vcard(&href, card, &credential, etag.as_deref())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The DELETE half of a contact write, with the same `If-Match` guard.
async fn delete_contact(goa_client: GoaClient, account: GoaContactsAccount, href: String, etag: Option<String>) -> Result<(), String> {
    let (client, credential) = carddav_connection(&goa_client, &account).await?;
    client.delete_contact_vcard(&href, &credential, etag.as_deref()).await.map_err(|e| e.to_string())
}

/// The collection-relative href for a brand-new contact: `<uid>.vcf` under
/// the book's href, the same client-generated-name convention as the
/// calendar write path's `<uid>.ics`. The UID is made URL-safe so an
/// email-style UID (`c1@example.com`) can't smuggle a path separator into
/// the href - the server never parses the UID back out of it.
fn new_contact_href(book_href: &str, card: &VCard) -> String {
    let base = book_href.trim_end_matches('/');
    let uid = card.uid.as_deref().unwrap_or("");
    let mut safe = String::with_capacity(uid.len());
    let mut kept_alnum = false;
    for c in uid.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            kept_alnum |= c.is_ascii_alphanumeric();
            safe.push(c);
        } else {
            safe.push('_');
        }
    }
    if !kept_alnum {
        safe = "contact".to_string();
    }
    format!("{base}/{safe}.vcf")
}

/// Builds a full `ContactsAccountSnapshot` from the account's cache: the
/// stored books (with their display names) and every cached contact, with
/// suggestions flattened and deduplicated. Returns `None` when there's
/// nothing cached worth painting (no books ever stored) - the caller then
/// falls back to live data or an empty snapshot.
fn snapshot_from_cache(account: &GoaContactsAccount, cache: &ContactsCache) -> Option<ContactsAccountSnapshot> {
    let books: Vec<lookout_dav::AddressBookInfo> = cache
        .load_address_books()
        .ok()?
        .into_iter()
        .map(|b| lookout_dav::AddressBookInfo {
            account_id: account.account_id.clone(),
            display_name: b.display_name,
            href: b.href,
        })
        .collect();
    if books.is_empty() {
        return None;
    }
    let contacts: Vec<lookout_dav::ContactRecord> = cache
        .load_contacts()
        .ok()?
        .into_iter()
        .map(|c| lookout_dav::ContactRecord {
            href: c.href,
            etag: c.etag,
            card: c.card,
        })
        .collect();
    let addresses: Vec<EmailAddress> = contacts.iter().flat_map(|record| record.card.email_addresses()).collect();
    Some(ContactsAccountSnapshot {
        display_name: account.display_name.clone(),
        books,
        contacts,
        suggestions: dedupe_addresses(addresses, usize::MAX),
    })
}

/// Syncs one address book into the account cache, preferring an RFC 6578
/// `sync-collection` delta (using the book's stored token) over a full
/// `addressbook-multiget` refetch. Returns the *changed* records this poll
/// produced (for logging only - the full contact set the UI needs comes from
/// the cache afterwards). Token handling:
///
/// - A stored token that the server rejects (stale, or a cold-start refusal)
///   falls back to a full refetch; the refetch then re-acquires a token via a
///   cold-start `sync-collection`, which servers that support delta sync
///   (Nextcloud, per RFC 6578) accept. Google's CardDAV refuses cold starts
///   outright (see `fetch_addressbook_contacts`), so the token stays cleared
///   there and every poll is a full refetch - the pre-cache behaviour.
async fn sync_addressbook_into(
    client: &DavClient,
    credential: &Credential,
    book: &lookout_dav::AddressBookInfo,
    cache: &ContactsCache,
) -> Result<Vec<lookout_dav::ContactRecord>, String> {
    let stored_token = cache.sync_token(&book.href).map_err(|e| e.to_string())?;
    if stored_token.is_some() {
        match client.sync_addressbook_delta(book, credential, stored_token.as_deref()).await {
            Ok((records, deleted, next_token)) => {
                cache.apply_delta(&book.href, next_token, &records, &deleted).map_err(|e| e.to_string())?;
                return Ok(records);
            }
            Err(e) => {
                tracing::warn!("CardDAV sync-collection failed for {:?}, falling back to a full refetch: {e}", book.display_name);
            }
        }
    } else {
        // No token yet: try a cold-start sync-collection (returns every
        // member as "changed" plus the first token). Servers that refuse it
        // (Google) fall through to the multiget refetch.
        match client.sync_addressbook_delta(book, credential, None).await {
            Ok((records, deleted, next_token)) => {
                cache.apply_delta(&book.href, next_token, &records, &deleted).map_err(|e| e.to_string())?;
                return Ok(records);
            }
            Err(e) => {
                tracing::warn!("CardDAV cold-start sync-collection failed for {:?}, falling back to a full refetch: {e}", book.display_name);
            }
        }
    }
    // Full refetch (never a cold start, so `addressbook-multiget` needs no
    // filter semantics). Store the records; try to re-acquire a token for
    // the next poll, but don't fail the sync if the server refuses.
    let records = client.fetch_addressbook_contacts_with_meta(book, credential).await.map_err(|e| e.to_string())?;
    let fresh_token = client.sync_addressbook_delta(book, credential, None).await.ok().and_then(|(_, _, token)| token);
    cache.apply_delta(&book.href, fresh_token, &records, &[]).map_err(|e| e.to_string())?;
    Ok(records)
}

async fn fetch_carddav_contacts(goa_client: GoaClient, account: GoaContactsAccount, cache: Option<&ContactsCache>) -> Result<ContactsAccountSnapshot, String> {
    let (client, credential) = carddav_connection(&goa_client, &account).await?;
    let home = client.discover_addressbook_home(&credential).await.map_err(|e| e.to_string())?;
    let books = client.list_addressbooks(&home, &account.account_id, &credential).await.map_err(|e| e.to_string())?;
    tracing::debug!("CardDAV discovery for {}: home={home:?}, {} addressbook(s) found", account.display_name, books.len());

    if let Some(cache) = cache {
        let _ = cache.store_address_books(&books);
        // Drop cached books that vanished server-side so their members read
        // as deletions (the UI receiver diffs the previous snapshot against
        // this one) instead of lingering in the People screen forever.
        let live_hrefs: HashSet<&str> = books.iter().map(|b| b.href.as_str()).collect();
        if let Ok(cached_books) = cache.load_address_books() {
            for stale in cached_books.iter().filter(|b| !live_hrefs.contains(b.href.as_str())) {
                let gone: Vec<String> = cache
                    .load_contacts()
                    .ok()
                    .map(|contacts| contacts.iter().filter(|c| c.book_href == stale.href).map(|c| c.href.clone()).collect())
                    .unwrap_or_default();
                let _ = cache.apply_delta(&stale.href, None, &[], &gone);
            }
        }
    }

    for book in &books {
        let result = match cache {
            Some(cache) => sync_addressbook_into(&client, &credential, book, cache).await,
            None => client.fetch_addressbook_contacts_with_meta(book, &credential).await.map_err(|e| e.to_string()),
        };
        match result {
            Ok(records) => {
                tracing::debug!(
                    "CardDAV addressbook {:?} (href {:?}) returned {} changed vcard(s)",
                    book.display_name,
                    book.href,
                    records.len()
                );
            }
            Err(e) => {
                tracing::warn!("CardDAV addressbook sync failed for {:?}: {e}", book.display_name);
            }
        }
    }
    tracing::debug!("CardDAV sync for {} complete", account.display_name);

    // Full-set semantics: per-book deltas above only describe *changes*, so
    // the snapshot must be rebuilt from the cache the deltas were applied to
    // (any contact missing from it was genuinely deleted). Without a cache,
    // the fetched records are the whole set and can be used directly.
    if let Some(cache) = cache {
        if let Some(snapshot) = snapshot_from_cache(&account, cache) {
            return Ok(snapshot);
        }
    }
    let mut addresses = Vec::new();
    let mut contacts = Vec::new();
    for book in &books {
        match client.fetch_addressbook_contacts_with_meta(book, &credential).await {
            Ok(records) => {
                for record in &records {
                    addresses.extend(record.card.email_addresses());
                }
                contacts.extend(records);
            }
            Err(e) => {
                tracing::warn!("CardDAV addressbook fetch failed for {:?}: {e}", book.display_name);
            }
        }
    }
    tracing::debug!("CardDAV sync for {}: {} total vcard(s)", account.display_name, contacts.len());

    Ok(ContactsAccountSnapshot {
        display_name: account.display_name,
        books,
        contacts,
        suggestions: dedupe_addresses(addresses, usize::MAX),
    })
}

/// "Manage groups…" for the People screen. A group is a vCard `CATEGORIES`
/// tag shared by several contacts, so a group only exists on the server as
/// the tags its member cards carry - the dialog therefore edits *members*:
/// renaming a group re-tags every member card across every connected
/// account, deleting a group strips the tag from them. New groups can't be
/// "created" here for the same reason (an empty group has nothing to persist
/// on the server) - they come into being by typing a new name in a contact's
/// Groups field in the editor.
pub fn show_manage_groups_dialog(window: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, toast_overlay: &adw::ToastOverlay) {
    let dialog = gtk::Window::builder()
        .transient_for(window)
        .modal(true)
        .title("Manage groups")
        .default_width(460)
        .default_height(480)
        .build();

    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).build();
    let scroller = gtk::ScrolledWindow::builder().child(&list).vexpand(true).build();

    // The batched rename/delete path: every member card of the affected
    // group gets an `Update` command against its own account, and one
    // summary toast lands when all replies are in.
    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    let rebuild_handle = rebuild.clone();
    {
        let list = list.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }

            // Group -> member cards (account, href, etag, card), across
            // every connected account.
            let mut members: HashMap<String, Vec<(AccountId, lookout_dav::ContactRecord)>> = HashMap::new();
            let st = state.borrow();
            for (account_id, snapshot) in &st.contacts_by_account {
                for record in &snapshot.contacts {
                    for tag in &record.card.categories {
                        members.entry(tag.clone()).or_default().push((account_id.clone(), record.clone()));
                    }
                }
            }
            drop(st);

            if members.is_empty() {
                let hint = gtk::Label::builder()
                    .label("No groups yet - type a name in a contact's Groups field to start one.")
                    .wrap(true)
                    .halign(gtk::Align::Start)
                    .css_classes(["dim-label"])
                    .margin_top(12)
                    .margin_bottom(12)
                    .build();
                list.append(&hint);
                return;
            }

            let mut names: Vec<String> = members.keys().cloned().collect();
            names.sort_by_key(|name| name.to_lowercase());
            for name in names {
                let member_list = members.remove(&name).unwrap_or_default();
                let row = gtk::ListBoxRow::new();
                let row_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(10)
                    .margin_top(6)
                    .margin_bottom(6)
                    .margin_start(10)
                    .margin_end(10)
                    .build();

                let name_entry = gtk::Entry::builder().text(&name).build();
                name_entry.set_hexpand(true);

                let delete = gtk::Button::from_icon_name("user-trash-symbolic");
                delete.set_tooltip_text(Some("Remove from all contacts"));
                delete.add_css_class("flat");

                row_box.append(&name_entry);
                row_box.append(&delete);
                row.set_child(Some(&row_box));
                list.append(&row);

                // Rename: re-tag every member card.
                let state_rename = state.clone();
                let toast_rename = toast_overlay.clone();
                let rebuild = rebuild_handle.clone();
                let name_rename = name.clone();
                let member_list_rename = member_list.clone();
                name_entry.connect_activate(move |entry| {
                    let new_name = entry.text().trim().to_string();
                    if new_name.is_empty() || new_name == name_rename {
                        entry.set_text(&name_rename);
                        return;
                    }
                    let mut replies = Vec::new();
                    for (account_id, record) in &member_list_rename {
                        let mut card = record.card.clone();
                        card.categories = card.categories.iter().map(|tag| if tag == &name_rename { new_name.clone() } else { tag.clone() }).collect();
                        if let Some(rx) = dispatch_contact_command(&state_rename, account_id, move |reply| ContactCommand::Update {
                            href: record.href.clone(),
                            etag: record.etag.clone(),
                            card: card.clone(),
                            reply,
                        }) {
                            replies.push(rx);
                        }
                    }
                    toast_contact_batch(toast_rename.clone(), replies, "Group renamed");
                    (rebuild.borrow())();
                });

                // Delete: strip the tag from every member card.
                let state_delete = state.clone();
                let toast_delete = toast_overlay.clone();
                let rebuild = rebuild_handle.clone();
                let name_delete = name.clone();
                let member_list_delete = member_list.clone();
                delete.connect_clicked(move |_| {
                    let mut replies = Vec::new();
                    for (account_id, record) in &member_list_delete {
                        let mut card = record.card.clone();
                        card.categories.retain(|tag| tag != &name_delete);
                        if let Some(rx) = dispatch_contact_command(&state_delete, account_id, move |reply| ContactCommand::Update {
                            href: record.href.clone(),
                            etag: record.etag.clone(),
                            card: card.clone(),
                            reply,
                        }) {
                            replies.push(rx);
                        }
                    }
                    toast_contact_batch(toast_delete.clone(), replies, "Group removed from contacts");
                    (rebuild.borrow())();
                });
            }
        });
    }
    (rebuild.borrow())();

    let close_button = gtk::Button::with_label("Close");
    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| dialog.close());
    }
    close_button.set_halign(gtk::Align::End);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    content.append(&scroller);
    content.append(&close_button);
    dialog.set_child(Some(&content));
    dialog.present();
}

/// Awaits a batch of contact-write replies and shows one summary toast -
/// the batched counterpart of `send_contact_command`'s per-write toast.
fn toast_contact_batch(toast_overlay: adw::ToastOverlay, replies: Vec<async_channel::Receiver<Result<(), String>>>, success: &'static str) {
    glib::spawn_future_local(async move {
        let mut failures = 0;
        let mut skipped = 0;
        for reply in replies {
            match reply.recv().await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => failures += 1,
                Err(_) => skipped += 1,
            }
        }
        if failures > 0 {
            toast_overlay.add_toast(adw::Toast::new(&format!("{failures} change(s) couldn't be saved")));
        } else if skipped > 0 {
            toast_overlay.add_toast(adw::Toast::new("Some changes couldn't be saved"));
        } else {
            toast_overlay.add_toast(adw::Toast::new(success));
        }
    });
}
