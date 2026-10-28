//! The People screen: contact account discovery, per-account sync sessions
//! (CardDAV RFC 6578 `sync-collection` deltas, or Microsoft Graph for
//! Microsoft 365 accounts, both into the account's `ContactsCache`), the left
//! category tree and right contact list, and every contact dialog
//! (details/editor/import/export/groups). Extracted out of `window.rs` so
//! the contacts view has a real module home like `calendar_view` and
//! `config_view`. The mutable state it drives still lives on `UiState`
//! (the per-account snapshots, the `contact_cmd_tx` write channels, starred
//! favourites, and the deleted bucket); this module owns the sync loops and
//! the widgets built against them.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::{AccountId, ContactList, ContactListMember, ContactsProvider, EmailAddress, EmailField, Photo, VCard};
use lookout_dav::{CardDavAccountConfig, ContactRecord, ContactsCache, Credential, DavClient};
use lookout_goa::{ContactsAuthMethod, GoaClient, GoaContactsAccount, GoaMailAccount};
use lookout_graph::{ContactsClient, DirectoryPerson, GraphClient, GraphContact};

use crate::microsoft_oauth::MicrosoftOAuth;
use crate::window::{DiscoveredGoaAccount, UiState};
use crate::worker::Worker;

#[derive(Clone)]
pub struct ContactsAccountSnapshot {
    display_name: String,
    /// The address books discovered for this account - the write path's
    /// targets: creating a contact PUTs/POSTs it under a book's href/Graph
    /// folder id, and the contact editor's create mode offers the books as a
    /// picker.
    books: Vec<lookout_dav::AddressBookInfo>,
    /// Every vCard from every address book discovered for this account,
    /// flattened - the People screen buckets these by fixed criteria
    /// (all/favourites/lists/deleted) and by vCard `CATEGORIES` rather than by
    /// which address book they came from (see `refresh_contacts_category_ui`),
    /// so which book a card came from doesn't need tracking here. Each record
    /// carries the server-side `href`/`getetag` (the CardDAV href, or the
    /// Graph contact id/`changeKey`) the write path needs to PUT/DELETE it
    /// back.
    /// `pub(crate)` so the birthdays calendar can read the same contact
    /// set the People screen shows.
    pub(crate) contacts: Vec<lookout_dav::ContactRecord>,
    /// `pub(crate)` so the compose window's contact picker can list every
    /// known address without re-deriving it from `contacts`/vCards itself.
    pub(crate) suggestions: Vec<EmailAddress>,
    /// Every contact list (`KIND:group` vCard) in this account's address
    /// books, with its members already resolved to addresses. Offered in the
    /// composer's recipient autocomplete/picker (P-05) so a list can be
    /// inserted and expanded.
    pub(crate) contact_lists: Vec<ContactList>,
}

/// Resolves every contact list in `contacts` to a [`ContactList`] with member
/// addresses, following `urn:uuid:` member references to the referenced card's
/// primary address. Unresolved members (a card the server didn't send, or a
/// card with no `EMAIL`) are skipped; `mailto:` members are used verbatim and
/// duplicates are dropped.
pub(crate) fn resolve_contact_lists(contacts: &[lookout_dav::ContactRecord]) -> Vec<ContactList> {
    let by_uid: HashMap<&str, &VCard> = contacts.iter().filter_map(|record| record.card.uid.as_deref().map(|uid| (uid, &record.card))).collect();
    let mut lists = Vec::new();
    for record in contacts.iter().filter(|record| record.card.is_contact_list()) {
        let mut members = Vec::new();
        let mut seen = HashSet::new();
        for member in record.card.contact_list_member_refs() {
            let address = match member {
                ContactListMember::Uid(uid) => by_uid.get(uid.as_str()).and_then(|card| card.email_addresses().into_iter().next()),
                ContactListMember::Address(address) => Some(EmailAddress { name: None, address }),
            };
            let Some(address) = address else { continue };
            let key = address.address.trim().to_lowercase();
            if key.is_empty() || !seen.insert(key) {
                continue;
            }
            members.push(address);
        }
        lists.push(ContactList {
            name: record.card.contact_list_name(),
            members,
        });
    }
    lists
}

impl ContactsAccountSnapshot {
    /// The account label shown in the People screen. Read by the contact-merge
    /// dialog so its duplicate groups can name the account they belong to.
    pub(crate) fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Whether `other` would show the same People screen: the same name,
    /// books and records. `suggestions` and `contact_lists` are derived from
    /// the records, so they aren't compared. A poll that finds nothing new
    /// (the usual case) is dropped on this, skipping the full repaint.
    pub(crate) fn same_content(&self, other: &ContactsAccountSnapshot) -> bool {
        self.display_name == other.display_name && self.books == other.books && self.contacts == other.contacts
    }
}

/// A bare snapshot for tests: no books (the write path's pickers aren't
/// exercised), just the display name and the given contact records.
#[cfg(test)]
pub(crate) fn test_snapshot(display_name: &str, contacts: Vec<lookout_dav::ContactRecord>) -> ContactsAccountSnapshot {
    let contact_lists = resolve_contact_lists(&contacts);
    ContactsAccountSnapshot {
        display_name: display_name.to_string(),
        books: Vec::new(),
        contacts,
        suggestions: Vec::new(),
        contact_lists,
    }
}

/// A snapshot with the given address suggestions, for the Focused Inbox
/// contact-set tests in other modules (item 2.12).
#[cfg(test)]
pub(crate) fn test_snapshot_with_suggestions(suggestions: Vec<EmailAddress>) -> ContactsAccountSnapshot {
    ContactsAccountSnapshot {
        display_name: String::new(),
        books: Vec::new(),
        contacts: Vec::new(),
        suggestions,
        contact_lists: Vec::new(),
    }
}

/// Which bucket a left-pane row filters the right-hand contact list down to.
/// The first four are fixed, always-present rows under each account's
/// header; `Category` rows are dynamic, one per distinct vCard `CATEGORIES`
/// value found across every connected account (see `refresh_contacts_category_ui`).
/// `Directory` is a marker, not a stored-contact bucket (item 4.7.8): its
/// member list is always empty, and `rebuild_contacts_list_ui` renders the
/// live `DirectoryPerson` results from `UiState` instead.
#[derive(Clone)]
enum ContactsBucketKind {
    AllContacts,
    Favourites,
    ContactLists,
    Deleted,
    Category(String),
    /// One of the desktop's address books (E1.5), by display name.
    DesktopBook(String),
    /// The live company-directory search (item 4.7.8).
    Directory,
}

fn contacts_bucket_label(kind: &ContactsBucketKind) -> String {
    match kind {
        ContactsBucketKind::AllContacts => "Your Contacts".to_string(),
        ContactsBucketKind::Favourites => "Favourites".to_string(),
        ContactsBucketKind::ContactLists => "Your contact lists".to_string(),
        ContactsBucketKind::Deleted => "Deleted".to_string(),
        ContactsBucketKind::Directory => "Directory".to_string(),
        ContactsBucketKind::Category(name) | ContactsBucketKind::DesktopBook(name) => name.clone(),
    }
}

/// One contact in a People bucket: its account, the account's label and the
/// record. Label and record are shared, so a contact that sits in several
/// buckets (its account's, "Your contacts", Favourites, each category) costs
/// a reference count per bucket rather than a deep copy.
pub(crate) type ContactMember = (AccountId, Rc<str>, Rc<ContactRecord>);

/// A `Category` bucket can span multiple accounts (the same tag used in two
/// address books), so contacts are tagged with their own account rather than
/// the choice carrying one account for all of them.
#[derive(Clone)]
pub struct ContactsCategoryChoice {
    kind: ContactsBucketKind,
    contacts: Vec<ContactMember>,
}

impl ContactsCategoryChoice {
    pub fn label(&self) -> String {
        contacts_bucket_label(&self.kind)
    }

    /// Whether this row is the live Directory search (item 4.7.8) rather than
    /// a stored-contact bucket. Its `contacts` list is always empty.
    pub(crate) fn is_directory(&self) -> bool {
        matches!(self.kind, ContactsBucketKind::Directory)
    }

    #[cfg(test)]
    pub(crate) fn members(&self) -> &[ContactMember] {
        &self.contacts
    }
}

/// The People screen's list filter (UX-65): the category to narrow to, plus
/// the "has email" / "has phone" quick filters, alongside the pane's search
/// box. Session state held on `UiState` and applied by
/// `rebuild_contacts_list_ui`.
#[derive(Clone, Default)]
pub struct ContactsFilterOptions {
    /// A `CATEGORIES` tag to narrow to, or `None` for every contact.
    pub category: Option<String>,
    pub has_email: bool,
    pub has_phone: bool,
}

impl ContactsFilterOptions {
    /// Whether `card` passes the quick filters (the search box is applied
    /// separately by `contact_matches_filter`).
    pub fn matches(&self, card: &VCard) -> bool {
        if let Some(category) = &self.category {
            if !card.categories.iter().any(|tag| tag.eq_ignore_ascii_case(category)) {
                return false;
            }
        }
        if self.has_email && card.emails.is_empty() {
            return false;
        }
        if self.has_phone && card.telephones.is_empty() {
            return false;
        }
        true
    }
}

/// The letter header a contact is filed under (UX-65): the uppercased first
/// character of its sort key, or `#` for a blank or non-alphabetic key -
/// Outlook's convention for digits, symbols and unnamed contacts.
pub fn contact_letter(key: &str) -> char {
    match key.trim().chars().next() {
        Some(c) if c.is_alphabetic() => c.to_uppercase().next().unwrap_or('#'),
        _ => '#',
    }
}

#[derive(Clone)]
pub struct ContactsListEntry {
    pub account_id: AccountId,
    pub account_label: String,
    pub category_label: String,
    /// The card plus the server-side object metadata the editor needs to
    /// PUT/DELETE it back (`If-Match` guards against clobbering), shared with
    /// the bucket it was listed from rather than copied.
    pub record: Rc<ContactRecord>,
}

impl ContactsListEntry {
    pub fn card(&self) -> &VCard {
        &self.record.card
    }

    /// Empty for entries that only ever display a card (the in-memory
    /// Deleted bucket, the desktop's books), where no write is possible.
    pub fn href(&self) -> &str {
        &self.record.href
    }

    pub fn etag(&self) -> Option<&str> {
        self.record.etag.as_deref()
    }
}

/// A write command for one account's contact session (CardDAV or Graph), sent
/// by the People screen's editor/import/manage-groups flows and executed by the
/// account's poll loop in `sync_contacts_account`. Each command carries a
/// one-shot `reply` channel the UI awaits for the result (a toast or a dialog
/// close), so success/failure can't be conflated with the sync polling's `tx`.
pub enum ContactCommand {
    /// Create a brand-new card: CardDAV PUTs `<uid>.vcf` under `book_href`
    /// with `If-None-Match: *`; Graph POSTs to the folder named by the href.
    Create {
        book_href: String,
        card: VCard,
        reply: async_channel::Sender<Result<(), String>>,
    },
    /// Update an edited card at its own `href`, with `etag` as the `If-Match`
    /// precondition (a stale etag fails with HTTP 412 instead of clobbering).
    Update {
        href: String,
        etag: Option<String>,
        card: VCard,
        reply: async_channel::Sender<Result<(), String>>,
    },
    /// Delete the card at `href`, with `etag` as `If-Match`.
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

/// The most Graph contact photos fetched in one sync pass. Graph returns the
/// photo from a separate `/photo/$value` request (not inline in the contact
/// JSON), so a large fresh address book would otherwise issue one request per
/// contact at once; the cap spreads them across polls (photos are cached as
/// the card's `PHOTO`, so a fetched one is never re-requested).
const MAX_GRAPH_PHOTO_FETCHES_PER_SYNC: usize = 50;
fn contacts_match_prefix(contact: &EmailAddress, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let needle = prefix.to_lowercase();
    contact.address.to_lowercase().starts_with(&needle) || contact.name.as_deref().is_some_and(|name| name.to_lowercase().starts_with(&needle))
}

/// A contact list matches a completion prefix on its name alone - it carries
/// no address of its own, only its members.
pub(crate) fn contact_list_matches_prefix(list: &ContactList, prefix: &str) -> bool {
    prefix.is_empty() || list.name.to_lowercase().starts_with(&prefix.to_lowercase())
}

/// Whether a contact matches the People pane's search box (P-06): the query
/// is matched case-insensitively as a substring of the display name, any
/// email address, or the organisation. `query` is expected pre-lowercased.
pub(crate) fn contact_matches_filter(card: &VCard, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if vcard_display_name(card).to_lowercase().contains(query) {
        return true;
    }
    if card.emails.iter().any(|email| email.address.to_lowercase().contains(query)) {
        return true;
    }
    card.organization.as_ref().is_some_and(|org| org.iter().any(|part| part.to_lowercase().contains(query)))
}

pub(crate) fn vcard_display_name(card: &VCard) -> String {
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

pub(crate) fn vcard_primary_email(card: &VCard) -> String {
    card.emails.first().map(|email| email.address.clone()).unwrap_or_else(|| "No email address".to_string())
}

/// The contact's `PHOTO` as a decoded texture, when it carries embedded bytes
/// the image loader can decode. A URI-only photo yields `None` (remote images
/// aren't fetched here), as does a decode failure - the caller falls back to
/// coloured initials.
pub(crate) fn contact_photo_texture(photo: &lookout_core::Photo) -> Option<gtk::gdk::Texture> {
    let data = photo.data.clone()?;
    gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(data)).ok()
}

/// Renders a birthday/anniversary for the details dialog: the yearless form
/// (`MM-DD`) when the year is omitted, else the full ISO date.
fn format_contact_date(value: lookout_core::Birthday) -> String {
    if value.omit_year {
        value.date.format("%m-%d").to_string()
    } else {
        value.date.format("%Y-%m-%d").to_string()
    }
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

    if !card.urls.is_empty() {
        lines.push("Websites:".to_string());
        for url in &card.urls {
            lines.push(format!("  - {url}"));
        }
    }

    if !card.impp.is_empty() {
        lines.push("Instant messaging:".to_string());
        for impp in &card.impp {
            let types = if impp.types.is_empty() {
                "".to_string()
            } else {
                format!(" ({})", impp.types.join(", "))
            };
            lines.push(format!("  - {}{}", impp.uri, types));
        }
    }

    if let Some(birthday) = card.birthday {
        lines.push(format!("Birthday: {}", format_contact_date(birthday)));
    }
    if let Some(anniversary) = card.anniversary {
        lines.push(format!("Anniversary: {}", format_contact_date(anniversary)));
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
    // Custom/unknown properties are shown read-only, last.
    for property in &card.other {
        if !property.value.trim().is_empty() {
            lines.push(format!("{}: {}", property.name, property.value.trim()));
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
        .title(vcard_display_name(entry.card()))
        .default_width(520)
        .default_height(420)
        .build();

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();

    let details = gtk::Label::builder()
        .label(vcard_details_text(&entry.account_label, &entry.category_label, entry.card()))
        .xalign(0.0)
        .yalign(0.0)
        .selectable(true)
        .wrap(true)
        .build();
    // The details block is exactly the contact's personal data.
    crate::privacy::mark(&details);
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
pub(crate) fn dispatch_contact_command(
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

/// Saves an edited contact back to its own account/session. The People
/// details pane's inline editor (UX-65) uses this; the dialog goes through
/// `route_contact_update` on the same path. A display-only entry (the Deleted
/// bucket, `href` empty) can't be written and is skipped.
pub fn save_contact_entry(state: &Rc<RefCell<UiState>>, toast_overlay: &adw::ToastOverlay, entry: &ContactsListEntry, card: VCard) {
    if entry.href().is_empty() {
        toast_overlay.add_toast(adw::Toast::new("This contact can't be saved here."));
        return;
    }
    route_contact_update(
        state,
        entry.account_id.clone(),
        entry.href().to_string(),
        entry.etag().map(str::to_string),
        card,
        toast_overlay,
    );
}

/// Routes a deleted contact to its account's session (`Delete` with the
/// etag as `If-Match`). The session's post-write resync lands the card in
/// the Deleted bucket via the usual diff.
fn route_contact_delete(state: &Rc<RefCell<UiState>>, account_id: AccountId, href: String, etag: Option<String>, toast_overlay: &adw::ToastOverlay) {
    send_contact_command(state, &account_id, toast_overlay, "Contact deleted", "Couldn't delete contact", move |reply| {
        ContactCommand::Delete { href, etag, reply }
    });
}

/// Deletes every writable contact in `entries` through its account's
/// session, toasting a single aggregate outcome instead of one toast per
/// card. Deleted-bucket cards (`href` empty, display-only) are skipped.
pub fn delete_contact_entries(state: &Rc<RefCell<UiState>>, toast_overlay: &adw::ToastOverlay, entries: &[ContactsListEntry]) {
    let writable: Vec<ContactsListEntry> = entries.iter().filter(|entry| !entry.href().is_empty()).cloned().collect();
    if writable.is_empty() {
        toast_overlay.add_toast(adw::Toast::new("No deletable contacts selected."));
        return;
    }
    let mut replies = Vec::new();
    for entry in &writable {
        let Some(reply_rx) = dispatch_contact_command(state, &entry.account_id, move |reply| ContactCommand::Delete {
            href: entry.href().to_string(),
            etag: entry.etag().map(str::to_string),
            reply,
        }) else {
            toast_overlay.add_toast(adw::Toast::new("The contacts session isn't running."));
            continue;
        };
        replies.push(reply_rx);
    }
    if replies.is_empty() {
        return;
    }
    let toast_overlay = toast_overlay.clone();
    glib::spawn_future_local(async move {
        let mut ok = 0usize;
        let mut failures = Vec::new();
        for reply_rx in replies {
            match reply_rx.recv().await {
                Ok(Ok(())) => ok += 1,
                Ok(Err(message)) => failures.push(message),
                // The session died mid-write; its error handling owns the toast.
                Err(_) => {}
            }
        }
        if failures.is_empty() {
            let text = if ok == 1 { "Contact deleted".to_string() } else { format!("{ok} contacts deleted") };
            toast_overlay.add_toast(adw::Toast::new(&text));
        } else {
            let text = glib::markup_escape_text(&format!("Couldn't delete {} contact(s): {}", failures.len(), failures.join("; ")));
            toast_overlay.add_toast(adw::Toast::new(&text));
        }
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
            existing: Some(entry.card()),
            prefill: None,
            href: entry.href(),
            etag: entry.etag(),
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
            prefill: None,
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

/// Finds the contact whose vCard carries `address` (case-insensitive exact
/// match on the address string) across every connected account's snapshot,
/// scanning `preferred_account` first and the rest in display-name order.
/// Returns the owning account and a [`ContactsListEntry`] the editor flows
/// can act on; `None` when no contact has that address.
pub fn find_contact_by_address(state: &Rc<RefCell<UiState>>, address: &str, preferred_account: Option<&AccountId>) -> Option<(AccountId, ContactsListEntry)> {
    let needle = address.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let st = state.borrow();
    let mut accounts: Vec<(&AccountId, &ContactsAccountSnapshot)> = st.contacts_by_account.iter().collect();
    accounts.sort_by_key(|(_, snapshot)| snapshot.display_name.to_lowercase());
    // The email's own account scans first - its contacts are the most likely
    // home for the sender - and the rest follow in display-name order.
    if let Some(preferred) = preferred_account {
        if let Some(pos) = accounts.iter().position(|(id, _)| *id == preferred) {
            let (id, snapshot) = accounts.remove(pos);
            accounts.insert(0, (id, snapshot));
        }
    }
    for (account_id, snapshot) in accounts {
        for record in &snapshot.contacts {
            let matched = record.card.emails.iter().any(|email| email.address.trim().to_lowercase() == needle);
            if matched {
                return Some((
                    account_id.clone(),
                    ContactsListEntry {
                        account_id: account_id.clone(),
                        account_label: snapshot.display_name.clone(),
                        category_label: contacts_bucket_label(&ContactsBucketKind::AllContacts),
                        record: Rc::new(record.clone()),
                    },
                ));
            }
        }
    }
    None
}

/// Opens the contact editor's create form prefilled with `sender` - the
/// reading pane's "View contact" path when no address book has that address.
/// The address-book picker offers every connected account's books, ordered
/// with `preferred_account`'s (the account the email lives in) first so the
/// dropdown defaults to them; creating routes to whichever account owns the
/// picked book.
pub fn show_create_contact_for(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    toast_overlay: &adw::ToastOverlay,
    sender: &EmailAddress,
    preferred_account: Option<AccountId>,
) {
    // The seed card: name from the sender (falling back to the address, so
    // the form saves as-is when no display name came with it) and one email
    // row carrying the sender's address. A fresh UID per opened dialog keeps
    // repeated creates on distinct server hrefs.
    let seed = VCard {
        version: "4.0".to_string(),
        kind: None,
        uid: Some(uuid::Uuid::new_v4().to_string()),
        full_name: Some(sender.name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&sender.address).to_string()),
        name: None,
        organization: None,
        title: None,
        file_as: None,
        photo: None,
        emails: vec![EmailField {
            types: vec!["work".to_string()],
            address: sender.address.clone(),
        }],
        telephones: Vec::new(),
        addresses: Vec::new(),
        urls: Vec::new(),
        impp: Vec::new(),
        note: None,
        birthday: None,
        anniversary: None,
        categories: Vec::new(),
        other: Vec::new(),
    };
    show_create_contact_with_seed(window, state, toast_overlay, seed, preferred_account);
}

/// Opens the create form prefilled from a company-directory hit (item 4.7.8),
/// keeping the title and company the directory knows. A person already in the
/// stored contacts is reported rather than duplicated.
pub fn show_create_contact_from_directory(window: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, toast_overlay: &adw::ToastOverlay, person: &DirectoryPerson) {
    let address = person.address.trim();
    if address.is_empty() {
        return;
    }
    if let Some((_, existing)) = find_contact_by_address(state, address, None) {
        let name = vcard_display_name(existing.card());
        let text = glib::markup_escape_text(&format!("{name} is already in your contacts"));
        toast_overlay.add_toast(adw::Toast::new(&text));
        return;
    }
    let seed = directory_person_seed(person);
    show_create_contact_with_seed(window, state, toast_overlay, seed, None);
}

/// The shared "new contact" flow behind `show_create_contact_for` and the
/// Directory row's Add to contacts: check there is a writable book, collect
/// every account's books (preferring `preferred_account`'s), and open the
/// editor with `seed`.
fn show_create_contact_with_seed(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    toast_overlay: &adw::ToastOverlay,
    seed: VCard,
    preferred_account: Option<AccountId>,
) {
    let writable = {
        let st = state.borrow();
        st.contacts_by_account.values().any(|snapshot| !snapshot.books.is_empty())
    };
    if !writable {
        toast_overlay.add_toast(adw::Toast::new("Connect a contacts account to create contacts."));
        return;
    }
    let (books, book_accounts, fallback_account) = {
        let st = state.borrow();
        let mut accounts: Vec<(&AccountId, &ContactsAccountSnapshot)> = st.contacts_by_account.iter().collect();
        accounts.sort_by_key(|(_, snapshot)| snapshot.display_name.to_lowercase());
        // The email's own account's books come first, so the picker defaults
        // to them (the dropdown opens on index 0); the rest follow in
        // display-name order.
        if let Some(preferred) = &preferred_account {
            if let Some(pos) = accounts.iter().position(|(id, _)| *id == preferred) {
                let (id, snapshot) = accounts.remove(pos);
                accounts.insert(0, (id, snapshot));
            }
        }
        accounts.retain(|(_, snapshot)| !snapshot.books.is_empty());
        // Book labels get the account's name prefixed when several accounts
        // contribute, so the picker tells books apart.
        let multi_account = accounts.len() > 1;
        let mut books: Vec<(String, String)> = Vec::new();
        let mut book_accounts: HashMap<String, AccountId> = HashMap::new();
        for (account_id, snapshot) in &accounts {
            for book in &snapshot.books {
                let label = if multi_account {
                    format!("{} · {}", snapshot.display_name, book.display_name)
                } else {
                    book.display_name.clone()
                };
                books.push((label, book.href.clone()));
                book_accounts.insert(book.href.clone(), (*account_id).clone());
            }
        }
        let fallback_account = accounts[0].0.clone();
        (books, book_accounts, fallback_account)
    };
    let state = state.clone();
    let toast_overlay = toast_overlay.clone();
    crate::contacts_editor::show_contact_editor(
        window,
        crate::contacts_editor::ContactEditorPrefill {
            books: &books,
            existing: None,
            prefill: Some(&seed),
            href: "",
            etag: None,
        },
        {
            let state = state.clone();
            let toast_overlay = toast_overlay.clone();
            move |book_href, card| {
                let account_id = book_accounts.get(&book_href).cloned().unwrap_or_else(|| fallback_account.clone());
                route_contact_create(&state, account_id, book_href, card, &toast_overlay);
            }
        },
        // Unreachable on the blank form; update/delete only fire for an edit.
        move |_href, _etag, _card| {},
        move |_href, _etag| {},
    );
}

/// The contact-file formats the People screen reads and writes (P-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactFileFormat {
    /// A multi-card `.vcf` file.
    VCard,
    /// Outlook's "Comma Separated Values (Windows)" export
    /// ([`lookout_core::interop::contacts_csv`]).
    OutlookCsv,
}

/// The format implied by a chosen file's extension: `.csv` (any case) is
/// Outlook CSV, everything else goes through the vCard path - the same split
/// the import chooser's filter offers.
fn contact_file_format_for_path(path: &std::path::Path) -> ContactFileFormat {
    if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("csv")) {
        ContactFileFormat::OutlookCsv
    } else {
        ContactFileFormat::VCard
    }
}

/// The cards a chosen file holds, one `Result` per source entry so the
/// import loop can count a malformed card as skipped. The CSV reader is
/// all-or-nothing on the header; individual bad cells degrade inside a card
/// instead (see `contacts_csv`).
fn contact_cards(format: ContactFileFormat, text: &str) -> Vec<Result<VCard, ()>> {
    match format {
        ContactFileFormat::VCard => lookout_core::VCard::parse_all(text).into_iter().map(|card| card.map_err(|_| ())).collect(),
        ContactFileFormat::OutlookCsv => lookout_core::interop::contacts_csv::parse(text)
            .map(|cards| cards.into_iter().map(Ok).collect())
            .unwrap_or_default(),
    }
}

/// "Import…" for the People screen: pick a `.vcf` or Outlook CSV file, then
/// route every contact in it to a chosen address book (the format follows
/// the file's extension). Deduplication is per the accepted policy: a card
/// whose `UID` matches an existing contact in the target *account* updates
/// that contact in place (its server href/etag are kept, so the write is
/// guarded like an edit); a card without a matching UID but with an email
/// the account already has is skipped and counted; anything else is created.
/// One summary toast reports the three counts when every write has answered.
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
    // It becomes the chosen file's name, so keep it content, not chrome.
    crate::privacy::mark_content(&file_label);
    let chosen_file: Rc<RefCell<Option<(ContactFileFormat, String)>>> = Rc::new(RefCell::new(None));

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
    // Address-book names are content: blurred by privacy mode.
    crate::privacy::mark_content(&book_dropdown);
    let target_group = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    let target_label = gtk::Label::builder().label("Import into").xalign(0.0).build();
    crate::privacy::mark_ui(&target_label);
    target_group.append(&target_label);
    target_group.append(&book_dropdown);

    let import_button = gtk::Button::with_label("Import");
    import_button.add_css_class("suggested-action");
    import_button.set_sensitive(false);
    let close_button = gtk::Button::with_label("Close");

    {
        let window = window.clone();
        let chosen_file = chosen_file.clone();
        let file_label = file_label.clone();
        let import_button = import_button.clone();
        choose_button.connect_clicked(move |_| {
            let window = window.clone();
            let chosen_file = chosen_file.clone();
            let file_label = file_label.clone();
            let import_button = import_button.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_suffix("vcf");
                filter.add_suffix("vcard");
                filter.add_suffix("csv");
                filter.set_name(Some("Contact files (*.vcf, *.vcard, *.csv)"));
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let picker = gtk::FileDialog::builder().title("Choose a contact file").filters(&filters).build();
                let Ok(file) = picker.open_future(Some(&window)).await else { return };
                let Some(path) = file.path() else { return };
                let Ok(raw) = std::fs::read(&path) else { return };
                let text = String::from_utf8_lossy(&raw).into_owned();
                let format = contact_file_format_for_path(&path);
                let card_count = match format {
                    ContactFileFormat::VCard => lookout_core::VCard::parse_all(&text).len(),
                    ContactFileFormat::OutlookCsv => lookout_core::interop::contacts_csv::parse(&text).map(|cards| cards.len()).unwrap_or(0),
                };
                if card_count == 0 {
                    *chosen_file.borrow_mut() = None;
                    file_label.set_label("No contacts found in this file.");
                    import_button.set_sensitive(false);
                    return;
                }
                *chosen_file.borrow_mut() = Some((format, text));
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
        let chosen_file = chosen_file.clone();
        let books = books.clone();
        let book_dropdown = book_dropdown.clone();
        import_button.connect_clicked(move |_| {
            let Some((format, text)) = chosen_file.borrow().clone() else { return };
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
            for result in contact_cards(format, &text) {
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
                    let summary = glib::markup_escape_text(&format!("{summary} ({failures} failed)"));
                    toast_overlay.add_toast(adw::Toast::new(&summary));
                } else {
                    let summary = glib::markup_escape_text(&summary);
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
/// `parse_all` imports), or as an Outlook CSV file (P-08, §4.6), per
/// `format`.
pub fn export_current_contacts(window: &adw::ApplicationWindow, format: ContactFileFormat, entries: &Rc<RefCell<Vec<ContactsListEntry>>>, toast_overlay: &adw::ToastOverlay) {
    let cards: Vec<VCard> = entries.borrow().iter().map(|entry| entry.card().clone()).collect();
    if cards.is_empty() {
        toast_overlay.add_toast(adw::Toast::new("Nothing to export in this category."));
        return;
    }
    save_contact_cards(window, format, cards, toast_overlay);
}

/// "Export Contacts…" for the File page (Open & Export): saves every contact
/// in every address book of every account, whatever the People screen's
/// current category is - the whole address book rather than a slice of it.
pub fn export_all_contacts(window: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, format: ContactFileFormat, toast_overlay: &adw::ToastOverlay) {
    let cards: Vec<VCard> = {
        let state = state.borrow();
        let mut cards = Vec::new();
        let mut seen = HashSet::new();
        for snapshot in state.contacts_by_account.values() {
            for record in &snapshot.contacts {
                // A UID is unique within an account, and a server href is
                // unique always; either keeps an account synced through more
                // than one pass from exporting the same card twice.
                let key = record.card.uid.clone().unwrap_or_else(|| record.href.clone());
                if seen.insert(key) {
                    cards.push(record.card.clone());
                }
            }
        }
        cards
    };
    if cards.is_empty() {
        toast_overlay.add_toast(adw::Toast::new("No contacts to export yet."));
        return;
    }
    save_contact_cards(window, format, cards, toast_overlay);
}

/// The shared save half of both contact exports: asks where to write, then
/// writes one multi-card `.vcf` (the same shape `parse_all` imports) or an
/// Outlook CSV file (P-08, §4.6).
fn save_contact_cards(window: &adw::ApplicationWindow, format: ContactFileFormat, cards: Vec<VCard>, toast_overlay: &adw::ToastOverlay) {
    let window = window.clone();
    let toast_overlay = toast_overlay.clone();
    glib::spawn_future_local(async move {
        let filter = gtk::FileFilter::new();
        let (filter_name, initial_name) = match format {
            ContactFileFormat::VCard => {
                filter.add_suffix("vcf");
                ("vCard files (*.vcf)", "contacts.vcf")
            }
            ContactFileFormat::OutlookCsv => {
                filter.add_suffix("csv");
                ("Outlook CSV files (*.csv)", "contacts.csv")
            }
        };
        filter.set_name(Some(filter_name));
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Export contacts").filters(&filters).initial_name(initial_name).build();
        let Ok(file) = dialog.save_future(Some(&window)).await else { return };
        let Some(path) = file.path() else { return };
        let body = match format {
            ContactFileFormat::VCard => {
                let mut body = String::new();
                for card in &cards {
                    body.push_str(&card.serialize());
                    body.push_str("\r\n");
                }
                body
            }
            // Excel trusts a UTF-8 BOM, and Outlook writes one.
            ContactFileFormat::OutlookCsv => format!("\u{feff}{}", lookout_core::interop::contacts_csv::export(&cards)),
        };
        match std::fs::write(&path, body) {
            Ok(()) => toast_overlay.add_toast(adw::Toast::new(&format!("Exported {} contact(s)", cards.len()))),
            Err(e) => toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't export contacts: {e}")))),
        }
    });
}

pub(crate) fn dedupe_addresses(addresses: Vec<EmailAddress>, limit: usize) -> Vec<EmailAddress> {
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

/// A directory/GAL person matches a completion prefix on their display name or
/// address, the same rule `contacts_match_prefix` applies to a stored contact.
fn directory_matches_prefix(person: &DirectoryPerson, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let needle = prefix.to_lowercase();
    person.address.to_lowercase().starts_with(&needle) || person.name.as_deref().is_some_and(|name| name.to_lowercase().starts_with(&needle))
}

/// The label a directory hit shows in the People Directory list and details
/// header: its display name, falling back to the address when the directory
/// carries no name (item 4.7.8).
pub(crate) fn directory_person_display_name(person: &DirectoryPerson) -> String {
    person
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| person.address.trim())
        .to_string()
}

/// The secondary line for a directory hit: "Title · Company", or the address
/// when the directory knows neither.
pub(crate) fn directory_person_secondary(person: &DirectoryPerson) -> String {
    let parts: Vec<&str> = [person.title.as_deref(), person.company.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        person.address.trim().to_string()
    } else {
        parts.join(" · ")
    }
}

/// The seed card for "Add to contacts" on a directory hit (item 4.7.8): the
/// name, work email, title and company the directory gave us, under a fresh
/// UID so repeated creates land on distinct server hrefs.
pub(crate) fn directory_person_seed(person: &DirectoryPerson) -> VCard {
    let display_name = directory_person_display_name(person);
    let organization = person
        .company
        .as_deref()
        .map(str::trim)
        .filter(|company| !company.is_empty())
        .map(|company| vec![company.to_string()]);
    let title = person.title.as_deref().map(str::trim).filter(|title| !title.is_empty()).map(str::to_string);
    VCard {
        version: "4.0".to_string(),
        kind: None,
        uid: Some(uuid::Uuid::new_v4().to_string()),
        full_name: Some(display_name),
        name: None,
        organization,
        title,
        file_as: None,
        photo: None,
        emails: vec![EmailField {
            types: vec!["work".to_string()],
            address: person.address.trim().to_string(),
        }],
        telephones: Vec::new(),
        addresses: Vec::new(),
        urls: Vec::new(),
        impp: Vec::new(),
        note: None,
        birthday: None,
        anniversary: None,
        categories: Vec::new(),
        other: Vec::new(),
    }
}

pub fn merge_contact_suggestions(
    mail_history: Vec<EmailAddress>,
    carddav: &[EmailAddress],
    directory: &[DirectoryPerson],
    lists: &[ContactList],
    prefix: &str,
    limit: usize,
) -> Vec<crate::recipient_entry::Suggestion> {
    let mut addresses = Vec::new();
    addresses.extend(mail_history);
    addresses.extend(carddav.iter().filter(|c| contacts_match_prefix(c, prefix)).cloned());
    let mut merged: Vec<crate::recipient_entry::Suggestion> = dedupe_addresses(addresses, limit).into_iter().map(crate::recipient_entry::Suggestion::Address).collect();
    // Directory/GAL hits (item 4.7.1) come after stored contacts: a person the
    // user already has as a contact wins, and the directory only fills what's
    // left of `limit`. They keep their own variant so the picker and People
    // search can render them as read-only rows (4.7.7/4.7.8).
    let mut seen: HashSet<String> = merged
        .iter()
        .filter_map(|suggestion| match suggestion {
            crate::recipient_entry::Suggestion::Address(address) => Some(address.address.trim().to_lowercase()),
            _ => None,
        })
        .collect();
    for person in directory.iter().filter(|person| directory_matches_prefix(person, prefix)) {
        if merged.len() >= limit {
            break;
        }
        let Some(address) = crate::recipient_entry::directory_person_address(person) else {
            continue;
        };
        if !seen.insert(address.address.trim().to_lowercase()) {
            continue;
        }
        merged.push(crate::recipient_entry::Suggestion::Directory(person.clone()));
    }
    for list in lists.iter().filter(|list| contact_list_matches_prefix(list, prefix)) {
        if merged.len() >= limit {
            break;
        }
        merged.push(crate::recipient_entry::Suggestion::List(list.clone()));
    }
    merged
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
pub fn calendar_attendee_suggestions(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>) -> crate::recipient_entry::SuggestionSource {
    let state = state.clone();
    let worker = worker.clone();
    // The directory half (item 4.7.6) is unioned across every connected
    // account's configured directories, like the stored-contact half below -
    // an invitee isn't tied to the event's calendar account.
    let directory = crate::directory::directory_source(&state, None, worker.clone());
    let carddav_provider = SnapshotContactsProvider {
        state: state.clone(),
        account: None,
    };
    Rc::new(move |prefix: &str, complete: Rc<dyn Fn(Vec<crate::recipient_entry::Suggestion>)>| {
        let prefix = prefix.trim().to_string();
        let carddav = carddav_provider.search_contacts(&prefix, 8);
        let caches: Vec<Arc<lookout_mail::Cache>> = state.borrow().accounts.values().filter_map(|h| h.address_cache.clone()).collect();
        let receivers: Vec<async_channel::Receiver<Vec<EmailAddress>>> = caches
            .into_iter()
            .map(|cache| {
                let prefix = prefix.clone();
                crate::window::spawn_cache_read(&worker, cache, move |cache| cache.search_contacts(&prefix, 8))
            })
            .collect();
        // Deliver the merged stored-plus-directory list when the directory
        // answers, replacing the stored-contact-only delivery that goes first.
        let deliver_directory = {
            let directory = directory.clone();
            let complete = complete.clone();
            let carddav = carddav.clone();
            let prefix = prefix.clone();
            move |mail_history: Vec<EmailAddress>| {
                directory.search(
                    &prefix,
                    Rc::new({
                        let complete = complete.clone();
                        let carddav = carddav.clone();
                        let prefix = prefix.clone();
                        move |hits| complete(merge_contact_suggestions(mail_history.clone(), &carddav, &hits, &[], &prefix, 8))
                    }),
                );
            }
        };
        glib::spawn_future_local(async move {
            let mut mail_history = Vec::new();
            for rx in receivers {
                if let Ok(hits) = rx.recv().await {
                    mail_history.extend(hits);
                }
            }
            complete(merge_contact_suggestions(mail_history.clone(), &carddav, &[], &[], &prefix, 8));
            deliver_directory(mail_history);
        });
    })
}

/// A stable-ish key for a contact, used for UI state (`starred_contacts`,
/// deletion diffing, the contact-list multi-select) that has nowhere else
/// to hang off of: the vCard's `UID`, or its first email address if the
/// server didn't send one.
pub(crate) fn contact_identity(card: &VCard) -> String {
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
fn append_contacts_choice_row(list: &gtk::ListBox, label: &str, chrome: bool) -> gtk::ListBoxRow {
    let row_label = gtk::Label::builder().label(label).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    row_label.set_margin_start(20);
    row_label.set_margin_end(8);
    row_label.set_margin_top(6);
    row_label.set_margin_bottom(6);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_label));
    if chrome {
        crate::privacy::mark_ui(&row);
    }
    list.append(&row);
    row
}

/// Rebuilds the People screen's left pane: each connected account gets a
/// header followed by its four fixed buckets (Your Contacts/Favourites/Your
/// contact lists/Deleted), then the desktop's address books (E1.5) get one
/// read-only row each under a "Desktop" header, then every distinct vCard
/// `CATEGORIES` tag found across all accounts gets its own row under a
/// trailing "Categories" header. Header rows aren't part of `categories` and can't be selected;
/// each selectable row stashes its index into `categories` on itself (see
/// `contacts_category_list.connect_row_selected` in `build`), since a row's
/// position in the widget no longer matches its index in that list once
/// headers are mixed in.
#[allow(clippy::too_many_arguments)]
pub fn refresh_contacts_category_ui(
    state: &Rc<RefCell<UiState>>,
    category_list: &gtk::ListBox,
    contact_list: &gtk::ListBox,
    categories: &Rc<RefCell<Vec<ContactsCategoryChoice>>>,
    contacts: &Rc<RefCell<Vec<ContactsListEntry>>>,
    selected_index: Option<i32>,
    selected: &Rc<RefCell<HashSet<(AccountId, String)>>>,
    refresh_actions: &Rc<dyn Fn()>,
    details_refresh: &Rc<dyn Fn()>,
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
        accounts.sort_by_cached_key(|(_, data)| data.display_name.to_lowercase());

        // Every distinct CATEGORIES tag across every account, so the same
        // tag used by contacts in two different address books still gets
        // one combined row rather than one per account.
        let mut category_members: HashMap<String, Vec<ContactMember>> = HashMap::new();
        // The same three aggregates the fixed top-level buckets use, built
        // while walking the accounts so the second pass doesn't re-derive them.
        let mut all_contacts: Vec<ContactMember> = Vec::new();
        let mut all_lists: Vec<ContactMember> = Vec::new();
        let mut all_deleted: Vec<ContactMember> = Vec::new();
        // Each account's members and deleted cards, wrapped once here and
        // shared by the per-account rows below.
        let mut per_account: Vec<(Vec<ContactMember>, Vec<ContactMember>)> = Vec::with_capacity(accounts.len());

        for &(account_id, account) in &accounts {
            let label: Rc<str> = Rc::from(account.display_name.as_str());
            let account_contacts: Vec<ContactMember> = account.contacts.iter().map(|record| (account_id.clone(), label.clone(), Rc::new(record.clone()))).collect();

            for member in &account_contacts {
                for tag in &member.2.card.categories {
                    category_members.entry(tag.clone()).or_default().push(member.clone());
                }
                if member.2.card.is_contact_list() {
                    all_lists.push(member.clone());
                }
            }
            all_contacts.extend(account_contacts.iter().cloned());

            let mut deleted: Vec<ContactMember> = Vec::new();
            for card in st.deleted_contacts.get(account_id).into_iter().flatten() {
                deleted.push((
                    account_id.clone(),
                    label.clone(),
                    Rc::new(ContactRecord {
                        // Deleted-bucket cards are display-only (the diff
                        // against a poll only kept the card, not its server
                        // metadata): an empty href marks them as not writable,
                        // and the editor's Delete is disabled for them.
                        href: String::new(),
                        etag: None,
                        card: card.clone(),
                    }),
                ));
            }
            all_deleted.extend(deleted.iter().cloned());
            per_account.push((account_contacts, deleted));
        }

        // The desktop's address books (E1.5), read-only. Kept out of "Your
        // Contacts" (that's the accounts' contacts), but a starred desktop
        // contact still shows under Favourites.
        let desktop_books: Vec<(String, Vec<ContactMember>)> = st
            .desktop_contacts
            .iter()
            .map(|book| (book.display_name.clone(), crate::desktop_contacts::desktop_records(book)))
            .collect();
        let mut favourite_candidates = all_contacts.clone();
        favourite_candidates.extend(desktop_books.iter().flat_map(|(_, records)| records.iter().cloned()));

        // Outlook's fixed People rail, ahead of the per-account sections: the
        // four buckets over every account at once (UX-65), plus the live
        // Directory row when a GAL/LDAP source is configured (item 4.7.8).
        // Favourites keeps the full contact list - membership depends on
        // `starred_contacts`, which changes without a resync - so the filter
        // is applied live in `rebuild_contacts_list_ui`. The Directory row is
        // a marker with no members: its results are live `DirectoryPerson`s
        // rendered by `rebuild_directory_list_ui`.
        let mut fixed_kinds = vec![
            ContactsBucketKind::Favourites,
            ContactsBucketKind::AllContacts,
            ContactsBucketKind::ContactLists,
            ContactsBucketKind::Deleted,
        ];
        if crate::directory::directory_configured_for_state(&st) {
            fixed_kinds.push(ContactsBucketKind::Directory);
        }
        for kind in fixed_kinds {
            let members = match kind {
                ContactsBucketKind::Favourites => favourite_candidates.clone(),
                ContactsBucketKind::AllContacts => all_contacts.clone(),
                ContactsBucketKind::ContactLists => all_lists.clone(),
                ContactsBucketKind::Deleted => all_deleted.clone(),
                ContactsBucketKind::Directory => Vec::new(),
                ContactsBucketKind::Category(_) | ContactsBucketKind::DesktopBook(_) => continue,
            };
            let label = contacts_bucket_label(&kind);
            rows.push(ContactsCategoryChoice { kind, contacts: members });
            row_widgets.push((append_contacts_choice_row(category_list, &label, true), Some(rows.len() - 1)));
        }

        for (&(_, account), (account_contacts, deleted)) in accounts.iter().zip(per_account) {
            let lists: Vec<ContactMember> = account_contacts.iter().filter(|(_, _, record)| record.card.is_contact_list()).cloned().collect();

            // The header row shows the account's display name/address, so it
            // blurs under privacy mode ("Categories" below stays sharp).
            let account_header = append_contacts_header_row(category_list, &account.display_name);
            crate::privacy::mark(&account_header);
            row_widgets.push((account_header, None));

            // Favourites keeps the full, unfiltered contact list - which
            // contacts belong depends on `starred_contacts` (persisted in
            // `ui_state_db`) state that can change without a resync, so the
            // filter is applied live in `rebuild_contacts_list_ui` instead
            // of baked in here.
            for kind in [ContactsBucketKind::AllContacts, ContactsBucketKind::Favourites] {
                rows.push(ContactsCategoryChoice {
                    kind: kind.clone(),
                    contacts: account_contacts.clone(),
                });
                let label = contacts_bucket_label(&kind);
                row_widgets.push((append_contacts_choice_row(category_list, &label, true), Some(rows.len() - 1)));
            }

            rows.push(ContactsCategoryChoice {
                kind: ContactsBucketKind::ContactLists,
                contacts: lists,
            });
            row_widgets.push((
                append_contacts_choice_row(category_list, &contacts_bucket_label(&ContactsBucketKind::ContactLists), true),
                Some(rows.len() - 1),
            ));

            rows.push(ContactsCategoryChoice {
                kind: ContactsBucketKind::Deleted,
                contacts: deleted,
            });
            row_widgets.push((
                append_contacts_choice_row(category_list, &contacts_bucket_label(&ContactsBucketKind::Deleted), true),
                Some(rows.len() - 1),
            ));
        }

        if !desktop_books.is_empty() {
            let desktop_header = append_contacts_header_row(category_list, "Desktop");
            crate::privacy::mark_ui(&desktop_header);
            desktop_header.set_tooltip_text(Some("Address books from GNOME Contacts and Evolution - read-only here"));
            row_widgets.push((desktop_header, None));
            for (name, records) in desktop_books {
                // A book's name can be an address (a GOA account's), so it
                // blurs under privacy mode like an account header.
                let row = append_contacts_choice_row(category_list, &name, false);
                crate::privacy::mark(&row);
                rows.push(ContactsCategoryChoice {
                    kind: ContactsBucketKind::DesktopBook(name),
                    contacts: records,
                });
                row_widgets.push((row, Some(rows.len() - 1)));
            }
        }

        if !category_members.is_empty() {
            let categories_header = append_contacts_header_row(category_list, "Categories");
            crate::privacy::mark_ui(&categories_header);
            row_widgets.push((categories_header, None));
            let mut names: Vec<String> = category_members.keys().cloned().collect();
            names.sort_by_key(|name| name.to_lowercase());
            for name in names {
                let members = category_members.remove(&name).unwrap_or_default();
                rows.push(ContactsCategoryChoice {
                    kind: ContactsBucketKind::Category(name.clone()),
                    contacts: members,
                });
                row_widgets.push((append_contacts_choice_row(category_list, &name, false), Some(rows.len() - 1)));
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
        selected.borrow_mut().clear();
        refresh_actions();
        details_refresh();
        let label = gtk::Label::builder().label("No contacts available yet").xalign(0.0).css_classes(["dim-label"]).build();
        crate::privacy::mark_ui(&label);
        label.set_margin_start(10);
        label.set_margin_end(10);
        label.set_margin_top(10);
        label.set_margin_bottom(10);
        contact_list.append(&label);
        return;
    }

    // Default to "Your contacts" (the first aggregate bucket) rather than the
    // leading Favourites row, which is empty for most accounts.
    let default_index = categories
        .borrow()
        .iter()
        .position(|choice| matches!(choice.kind, ContactsBucketKind::AllContacts))
        .unwrap_or(0) as i32;
    let desired = selected_index.unwrap_or(default_index).clamp(0, categories.borrow().len() as i32 - 1);
    if let Some((row, _)) = row_widgets.iter().find(|(_, idx)| *idx == Some(desired as usize)) {
        category_list.select_row(Some(row));
    }
    rebuild_contacts_list_ui(contact_list, categories, contacts, desired, state, selected, refresh_actions, details_refresh);
}

/// Rebuilds the People screen's middle contact list for whichever bucket is
/// selected in the left pane. Takes `state` so each row can show (and toggle)
/// its favourite star live - Favourites membership is evaluated here rather
/// than pre-filtered in `refresh_contacts_category_ui` so starring/unstarring
/// a contact doesn't require rebuilding the whole left pane, just this list.
/// Rows are grouped under Outlook's alphabetical letter headers (UX-65), and
/// the pane's Filter/Sort options (`ContactsFilterOptions`) are applied here.
#[allow(clippy::too_many_arguments)]
pub fn rebuild_contacts_list_ui(
    contact_list: &gtk::ListBox,
    categories: &Rc<RefCell<Vec<ContactsCategoryChoice>>>,
    contacts: &Rc<RefCell<Vec<ContactsListEntry>>>,
    selected_category_index: i32,
    state: &Rc<RefCell<UiState>>,
    selected: &Rc<RefCell<HashSet<(AccountId, String)>>>,
    refresh_actions: &Rc<dyn Fn()>,
    details_refresh: &Rc<dyn Fn()>,
) {
    // Remember which contact was showing before the rebuild so the details
    // pane (and the list selection) can stay on it across a star toggle or a
    // sync repaint.
    let previous = contact_list
        .selected_row()
        .and_then(|row| unsafe { row.data::<usize>("contacts-entry-index") })
        .map(|idx| unsafe { *idx.as_ref() })
        .and_then(|idx| contacts.borrow().get(idx).map(|entry| (entry.account_id.clone(), contact_identity(entry.card()))));

    while let Some(child) = contact_list.first_child() {
        contact_list.remove(&child);
    }

    let Some(choice) = categories.borrow().get(selected_category_index as usize).cloned() else {
        state.borrow_mut().contacts_directory_active = false;
        contacts.borrow_mut().clear();
        selected.borrow_mut().clear();
        refresh_actions();
        details_refresh();
        return;
    };

    // The Directory row is a live search, not a stored-contact bucket
    // (item 4.7.8): render its `DirectoryPerson` results and leave the
    // stored list/selection empty, so the bulk actions stay insensitive.
    if choice.is_directory() {
        state.borrow_mut().contacts_directory_active = true;
        rebuild_directory_list_ui(contact_list, state, contacts, selected, refresh_actions, details_refresh);
        return;
    }
    state.borrow_mut().contacts_directory_active = false;

    let category_label = contacts_bucket_label(&choice.kind);
    let favourites_only = matches!(choice.kind, ContactsBucketKind::Favourites);
    let contact_lists_only = matches!(choice.kind, ContactsBucketKind::ContactLists);

    // The People pane's own search box (P-06) and Filter menu (UX-65) narrow
    // the list; the query, the filter options and the "File as" preference
    // all live on the shared state, so the row callbacks that trigger a
    // rebuild don't each have to thread them through.
    let (filter, file_as, options) = {
        let st = state.borrow();
        let query = st.contacts_filter.trim().to_lowercase();
        let pref = crate::settings::contact_file_as(&st.settings.get_string(crate::settings::CONTACTS_FILE_AS));
        (query, pref, st.contacts_filter_options.clone())
    };

    let mut entries: Vec<ContactsListEntry> = {
        let st = state.borrow();
        choice
            .contacts
            .into_iter()
            .filter(|(account_id, _, record)| !favourites_only || st.starred_contacts.contains(&(account_id.clone(), contact_identity(&record.card))))
            .filter(|(_, _, record)| options.matches(&record.card))
            .filter(|(_, _, record)| filter.is_empty() || contact_matches_filter(&record.card, &filter))
            .map(|(account_id, account_label, record)| ContactsListEntry {
                account_id,
                account_label: account_label.to_string(),
                category_label: category_label.clone(),
                record,
            })
            .collect()
    };
    entries.sort_by_cached_key(|entry| entry.card().file_as_key(file_as).to_lowercase());

    // Drop any selection whose contact isn't in this list - a category
    // switch, or an unstar removing a contact from Favourites - so the
    // checkboxes and the action bar never point at a contact that isn't
    // visible. Keyed by `(account_id, identity)`, so a star-toggle rebuild
    // (which re-sorts the same entries) leaves selections untouched.
    {
        let live: HashSet<(AccountId, String)> = entries.iter().map(|entry| (entry.account_id.clone(), contact_identity(entry.card()))).collect();
        selected.borrow_mut().retain(|key| live.contains(key));
    }

    if entries.is_empty() {
        if favourites_only {
            let icon = crate::window::svg_image(
                "/io/github/gavindi/Lookout/icons/no-favourite-contacts.svg",
                include_bytes!("../../../data/resources/icons/no-favourite-contacts.svg"),
                96,
            );
            icon.add_css_class("empty-state-icon");
            let label = gtk::Label::builder()
                .label("No stars in the sky. Makes me wonder why, I'm still gazing.")
                .wrap(true)
                .halign(gtk::Align::Center)
                .css_classes(["dim-label"])
                .build();
            crate::privacy::mark_ui(&label);
            let empty_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(12)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .build();
            empty_box.append(&icon);
            empty_box.append(&label);
            contact_list.append(&empty_box);
            if let Some(row) = empty_box.parent() {
                row.set_vexpand(true);
            }
        } else if contact_lists_only {
            let icon = crate::window::svg_image(
                "/io/github/gavindi/Lookout/icons/no-contact-groups.svg",
                include_bytes!("../../../data/resources/icons/no-contact-groups.svg"),
                96,
            );
            icon.add_css_class("empty-state-icon");
            let label = gtk::Label::builder()
                .label("Listen to all the translations of the stories across the sky\nWe drew our own constellations")
                .wrap(true)
                .halign(gtk::Align::Center)
                .css_classes(["dim-label"])
                .build();
            crate::privacy::mark_ui(&label);
            let empty_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(12)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .build();
            empty_box.append(&icon);
            empty_box.append(&label);
            contact_list.append(&empty_box);
            if let Some(row) = empty_box.parent() {
                row.set_vexpand(true);
            }
        } else {
            let label = gtk::Label::builder().label("No contacts in this category").xalign(0.0).css_classes(["dim-label"]).build();
            crate::privacy::mark_ui(&label);
            label.set_margin_start(10);
            label.set_margin_end(10);
            label.set_margin_top(10);
            label.set_margin_bottom(10);
            contact_list.append(&label);
        }
        contacts.borrow_mut().clear();
        refresh_actions();
        details_refresh();
        return;
    }

    // Rows grouped under their letter header (UX-65). The entry's index in
    // `entries` is stashed on its row because the widget position no longer
    // matches once header rows are interleaved.
    let mut row_widgets: Vec<(gtk::ListBoxRow, usize)> = Vec::new();
    let mut current_letter: Option<char> = None;
    for (entry_index, entry) in entries.iter().enumerate() {
        let sort_key = entry.card().file_as_key(file_as);
        let letter = contact_letter(&sort_key);
        if current_letter != Some(letter) {
            append_contacts_header_row(contact_list, &letter.to_string());
            current_letter = Some(letter);
        }

        let name = gtk::Label::builder()
            .label(vcard_display_name(entry.card()))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let email = gtk::Label::builder()
            .label(vcard_primary_email(entry.card()))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        // Contact names and addresses: blurred by privacy mode.
        crate::privacy::mark(&name);
        crate::privacy::mark(&email);
        let text_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        text_box.append(&name);
        text_box.append(&email);

        let identity = contact_identity(entry.card());
        // Multi-select checkbox, left of the avatar: toggling it adds the
        // contact to (or removes it from) `selected`, driving the ribbon's
        // Edit/Delete sensitivity. Deliberately a checkbox rather than
        // the `gtk::ListBox`'s own selection - the list stays single-select
        // and row-activation keeps opening the editor, so checking a box
        // never conflicts with opening a contact.
        let checkbox = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Select for multi-select actions")
            .build();
        checkbox.set_active(selected.borrow().contains(&(entry.account_id.clone(), identity.clone())));
        {
            let selected = selected.clone();
            let refresh_actions = refresh_actions.clone();
            let account_id = entry.account_id.clone();
            let identity = identity.clone();
            checkbox.connect_toggled(move |btn| {
                let key = (account_id.clone(), identity.clone());
                if btn.is_active() {
                    selected.borrow_mut().insert(key);
                } else {
                    selected.borrow_mut().remove(&key);
                }
                refresh_actions();
            });
        }
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
            let selected = selected.clone();
            let refresh_actions = refresh_actions.clone();
            let details_refresh = details_refresh.clone();
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
                rebuild_contacts_list_ui(
                    &contact_list,
                    &categories,
                    &contacts,
                    selected_category_index,
                    &state,
                    &selected,
                    &refresh_actions,
                    &details_refresh,
                );
            });
        }

        let display_name = vcard_display_name(entry.card());
        let primary_email = vcard_primary_email(entry.card());
        // The avatar: coloured initials underneath, the contact's `PHOTO` on
        // top when the card carries embedded bytes (same shape as the mail
        // list's sender avatar). A photo-less or undecodable card keeps the
        // initials.
        let avatar_label = gtk::Label::builder()
            .css_classes(["avatar-circle"])
            .label(crate::message_header::initials(Some(&display_name), &primary_email))
            .build();
        avatar_label.add_css_class(crate::message_header::avatar_color_class(&primary_email));
        let avatar_photo = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).css_classes(["avatar-photo"]).build();
        if let Some(texture) = entry.card().photo.as_ref().and_then(contact_photo_texture) {
            avatar_photo.set_paintable(Some(&texture));
        }
        let avatar = gtk::Overlay::builder()
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .child(&avatar_label)
            .build();
        avatar.add_overlay(&avatar_photo);
        // The avatar (initials and any photo) is identity-revealing.
        crate::privacy::mark(&avatar);

        let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
        row_box.set_margin_start(10);
        row_box.set_margin_end(10);
        row_box.set_margin_top(6);
        row_box.set_margin_bottom(6);
        row_box.append(&checkbox);
        row_box.append(&avatar);
        row_box.append(&text_box);
        row_box.append(&star_button);
        contact_list.append(&row_box);
        if let Some(row) = row_box.parent().and_then(|parent| parent.downcast::<gtk::ListBoxRow>().ok()) {
            unsafe { row.set_data("contacts-entry-index", entry_index) };
            row_widgets.push((row, entry_index));
        }
    }
    *contacts.borrow_mut() = entries;
    refresh_actions();

    // Keep the previous selection if it's still visible, else show the first
    // contact so the details pane is never blank while contacts exist.
    let target = previous
        .as_ref()
        .and_then(|key| {
            row_widgets
                .iter()
                .find(|(_, idx)| contact_identity(contacts.borrow()[*idx].card()) == key.1 && contacts.borrow()[*idx].account_id == key.0)
                .cloned()
        })
        .or_else(|| row_widgets.first().cloned());
    if let Some((row, _)) = target {
        contact_list.select_row(Some(&row));
    }
    details_refresh();
}

/// Renders the People screen's live Directory results (item 4.7.8) into the
/// middle list. Unlike `rebuild_contacts_list_ui` there are no stored
/// `ContactsListEntry`s here - the hits are `DirectoryPerson`s from
/// [`crate::directory::DirectorySource`], and the details pane renders the
/// selected one through `ContactsDetailsPane::set_directory_person`. The
/// stored list and checkbox selection are cleared so the ribbon's
/// Edit/Delete/Email actions stay insensitive; Send email and Add to contacts
/// live on the details pane instead.
fn rebuild_directory_list_ui(
    contact_list: &gtk::ListBox,
    state: &Rc<RefCell<UiState>>,
    contacts: &Rc<RefCell<Vec<ContactsListEntry>>>,
    selected: &Rc<RefCell<HashSet<(AccountId, String)>>>,
    refresh_actions: &Rc<dyn Fn()>,
    details_refresh: &Rc<dyn Fn()>,
) {
    while let Some(child) = contact_list.first_child() {
        contact_list.remove(&child);
    }
    contacts.borrow_mut().clear();
    selected.borrow_mut().clear();
    refresh_actions();

    let (query, searching, results, selected_index) = {
        let st = state.borrow();
        (
            st.contacts_directory_query.trim().to_string(),
            st.contacts_directory_searching,
            st.contacts_directory_results.clone(),
            st.contacts_directory_selected,
        )
    };

    let clear_selection = || {
        state.borrow_mut().contacts_directory_selected = None;
    };
    let show_message = |text: &str| {
        let label = gtk::Label::builder().label(text).xalign(0.0).wrap(true).css_classes(["dim-label"]).build();
        crate::privacy::mark_ui(&label);
        label.set_margin_start(10);
        label.set_margin_end(10);
        label.set_margin_top(10);
        label.set_margin_bottom(10);
        contact_list.append(&label);
    };

    if query.chars().count() < crate::directory::DIRECTORY_MIN_CHARS {
        clear_selection();
        show_message("Type at least three characters to search the directory");
        details_refresh();
        return;
    }
    if searching {
        clear_selection();
        show_message("Searching the directory…");
        details_refresh();
        return;
    }
    if results.is_empty() {
        clear_selection();
        show_message(&format!("No matches in the directory for “{query}”"));
        details_refresh();
        return;
    }

    let mut row_widgets: Vec<(gtk::ListBoxRow, usize)> = Vec::new();
    for (index, person) in results.iter().enumerate() {
        let display_name = directory_person_display_name(person);
        let secondary = directory_person_secondary(person);

        let name = gtk::Label::builder().label(&display_name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let detail = gtk::Label::builder()
            .label(&secondary)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        // Directory names, addresses and organisations are content.
        crate::privacy::mark(&name);
        crate::privacy::mark(&detail);
        let text_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        text_box.append(&name);
        text_box.append(&detail);

        let avatar_label = gtk::Label::builder()
            .css_classes(["avatar-circle"])
            .label(crate::message_header::initials(Some(&display_name), &person.address))
            .build();
        avatar_label.add_css_class(crate::message_header::avatar_color_class(&person.address));
        let avatar = gtk::Overlay::builder()
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .child(&avatar_label)
            .build();
        crate::privacy::mark(&avatar);

        let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        row_box.set_margin_start(10);
        row_box.set_margin_end(10);
        row_box.set_margin_top(6);
        row_box.set_margin_bottom(6);
        row_box.append(&avatar);
        row_box.append(&text_box);
        contact_list.append(&row_box);
        if let Some(row) = row_box.parent().and_then(|parent| parent.downcast::<gtk::ListBoxRow>().ok()) {
            unsafe { row.set_data("contacts-directory-index", index) };
            row_widgets.push((row, index));
        }
    }

    let target = selected_index.filter(|index| *index < row_widgets.len()).unwrap_or(0);
    state.borrow_mut().contacts_directory_selected = Some(target);
    if let Some((row, _)) = row_widgets.get(target) {
        contact_list.select_row(Some(row));
    }
    details_refresh();
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
    refresh_config: Rc<dyn Fn()>,
) {
    let (goa_tx, goa_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let result = async {
            let client = GoaClient::connect().await?;
            let accounts = client.list_contacts_accounts().await?;
            // Microsoft 365 mailboxes, listed independently of the contacts
            // interfaces: GOA's ms_graph provider may not expose a `Contacts`
            // interface at all, and this discovery can't rely on the mail
            // discovery having populated `goa_accounts` first (they race).
            let m365_mail: Vec<GoaMailAccount> = client.list_mail_accounts().await?.into_iter().filter(GoaMailAccount::is_microsoft_365).collect();
            Ok::<_, lookout_goa::Error>((client, accounts, m365_mail))
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
            Ok((client, accounts, m365_mail)) => {
                // The accounts a session was started for, so the Microsoft 365
                // fallback below doesn't double-connect one GOA already
                // reported a `Contacts` interface for.
                let mut started: HashSet<AccountId> = HashSet::new();
                for account in accounts {
                    let id = account.account_id.clone();
                    // Record the account in the GOA union (disabled ones
                    // included - Config's account list needs them), then
                    // sync only the enabled ones. A disabled account's
                    // contacts stay cached on disk, just unprocessed and
                    // hidden, ready for a re-enable.
                    let enabled = {
                        let mut st = state.borrow_mut();
                        let entry = st.goa_accounts.entry(id.clone()).or_insert(crate::window::DiscoveredGoaAccount {
                            display_name: String::new(),
                            email: String::new(),
                            provider_type: None,
                            mail: None,
                            calendar: None,
                            contacts: None,
                        });
                        entry.display_name = account.display_name.clone();
                        entry.email = account.display_name.clone();
                        entry.provider_type = account.provider_type.clone();
                        entry.contacts = Some(account.clone());
                        st.account_enabled(&id)
                    };
                    if !enabled {
                        continue;
                    }
                    // A Microsoft 365 account has no contacts until Graph is
                    // consented (item 5.2 / A-04); an unconsented one is
                    // recorded above but not synced, ready for a re-enable.
                    if !contacts_account_usable(&account) {
                        tracing::debug!("Microsoft 365 contacts {id} has no Outlook-sync consent yet; skipping until enabled in Config");
                        continue;
                    }
                    // Each account's session gets its own command channel,
                    // stored so the People screen's editor/import/group flows
                    // can route writes to the right account.
                    let (cmd_tx, cmd_rx) = async_channel::unbounded();
                    state.borrow_mut().contact_cmd_tx.insert(id.clone(), cmd_tx);
                    sync_contacts_account(
                        worker.clone(),
                        state.clone(),
                        toast_overlay.clone(),
                        client.clone(),
                        account,
                        cmd_rx,
                        refresh_contacts_ui.clone(),
                    );
                    started.insert(id);
                }
                // A Microsoft 365 mailbox whose `Contacts` interface GOA didn't
                // report is still served by the Graph contacts backend (item
                // 5.2 / A-04); synthesise its contacts account so the People
                // screen gains it as soon as Graph is consented.
                let fallback: Vec<GoaContactsAccount> = m365_mail
                    .into_iter()
                    .filter(|mail| !started.contains(&mail.account_id))
                    .filter(|mail| state.borrow().account_enabled(&mail.account_id))
                    .filter_map(|mail| {
                        let consented = crate::microsoft_oauth::MicrosoftOAuth::new(mail.account_id.clone()).graph_consented();
                        graph_contacts_account_from_mail(&mail, &mail.account_id, &mail.display_name, consented)
                    })
                    .collect();
                for account in fallback {
                    let (cmd_tx, cmd_rx) = async_channel::unbounded();
                    started.insert(account.account_id.clone());
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
                // The desktop's address books (E1.5) are read once the
                // synced accounts are known, so E1.3 can skip GOA copies; the
                // People screen shows when either has contacts, or when a
                // GAL/LDAP source is configured for its Directory row (4.7.8).
                let has_desktop = crate::desktop_contacts::reload_now(&state, &worker).await;
                let has_directory = crate::directory::directory_configured_for_state(&state.borrow());
                if started.is_empty() && !has_desktop && !has_directory {
                    show_page("contacts-empty");
                } else {
                    // Repaint now that the account list and directory config
                    // are known, so the rail's Directory row (4.7.8) can
                    // appear even when no contacts account loaded.
                    refresh_contacts_ui(None);
                    show_page("contacts");
                }
            }
            Err(e) => {
                if e.is_service_unavailable() {
                    state.borrow_mut().goa_unavailable = true;
                    refresh_config();
                } else {
                    toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't reach GNOME Online Accounts contacts: {e}"))));
                }
                // Without GOA there are no account contacts, but the desktop
                // may still have address books (E1.5) and a configured
                // directory still has its People row (4.7.8).
                let has_desktop = crate::desktop_contacts::reload_now(&state, &worker).await;
                let has_directory = crate::directory::directory_configured_for_state(&state.borrow());
                if has_desktop || has_directory {
                    refresh_contacts_ui(None);
                    show_page("contacts");
                } else {
                    show_page("contacts-empty");
                }
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
    // A Microsoft 365 account is served by the Graph contacts backend (item
    // 5.2 / A-04), not CardDAV; access is opt-in (Config's "Sync with Outlook"
    // runs the consent flow), and discovery skips an unconsented account.
    let graph_backend = account.is_microsoft_365();
    let (tx, rx) = async_channel::unbounded();
    let cache_account_id = account_id.clone();
    worker.spawn(async move {
        // The per-account contacts cache: fast-paint source at startup, the
        // full contact set deltas are applied to, and the RFC 6578 sync-token /
        // Graph delta-link store. Best-effort - a failed open just means no
        // cache (full refetches every poll, no fast paint), never an error.
        let cache = ContactsCache::open(&cache_account_id).ok();
        // The Graph transport, built once per session; `None` for CardDAV.
        let graph = graph_backend.then(|| {
            let oauth = std::sync::Arc::new(MicrosoftOAuth::new(account.account_id.clone()));
            ContactsClient::new(GraphClient::new(oauth))
        });
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
                        let result = match &graph {
                            Some(client) => graph_create_contact(client, &book_href, &card).await,
                            None => put_contact(goa_client.clone(), account.clone(), new_contact_href(&book_href, &card), &card, None).await,
                        };
                        let _ = reply.send(result.clone()).await;
                        if result.is_ok() {
                            resync = true;
                        }
                    }
                    ContactCommand::Update { href, etag, card, reply } => {
                        let result = match &graph {
                            Some(client) => graph_update_contact(client, &href, etag.as_deref(), &card).await,
                            None => put_contact(goa_client.clone(), account.clone(), href, &card, etag).await,
                        };
                        let _ = reply.send(result.clone()).await;
                        if result.is_ok() {
                            resync = true;
                        }
                    }
                    ContactCommand::Delete { href, etag, reply } => {
                        let result = match &graph {
                            Some(client) => graph_delete_contact(client, &href, etag.as_deref()).await,
                            None => delete_contact(goa_client.clone(), account.clone(), href, etag).await,
                        };
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
            let result = match &graph {
                Some(client) => fetch_graph_contacts(client, &account, cache.as_ref()).await,
                None => fetch_carddav_contacts(goa_client.clone(), account.clone(), cache.as_ref()).await,
            };
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
                    if state.borrow().contacts_by_account.get(&account_id).is_some_and(|previous| previous.same_content(&contacts)) {
                        continue;
                    }
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
                    // The local Focused Inbox classifier treats any contact as
                    // a known sender (M-03 / item 2.12); push the new union to
                    // every non-Microsoft session that doesn't already hold it.
                    crate::window::push_local_focus_contacts(&state);
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
    let contact_lists = resolve_contact_lists(&contacts);
    Some(ContactsAccountSnapshot {
        display_name: account.display_name.clone(),
        books,
        contacts,
        suggestions: dedupe_addresses(addresses, usize::MAX),
        contact_lists,
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

    let contact_lists = resolve_contact_lists(&contacts);
    Ok(ContactsAccountSnapshot {
        display_name: account.display_name,
        books,
        contacts,
        suggestions: dedupe_addresses(addresses, usize::MAX),
        contact_lists,
    })
}

/// Syncs one Microsoft 365 account's contacts from Graph (item 5.2 / A-04) into
/// the account cache, then rebuilds the snapshot from it - the same full-set
/// semantics as the CardDAV path, so the People screen's Deleted bucket sees a
/// vanished contact. Each Graph contact folder is one address book; contacts
/// are delta-synced per folder, falling back to a full read when the delta
/// cursor can't be used (a cold start on a server that refuses one, or any
/// per-folder delta error).
async fn fetch_graph_contacts(client: &ContactsClient, account: &GoaContactsAccount, cache: Option<&ContactsCache>) -> Result<ContactsAccountSnapshot, String> {
    let folders = client.list_folders().await.map_err(|e| e.to_string())?;
    let books: Vec<lookout_dav::AddressBookInfo> = folders
        .iter()
        .map(|folder| lookout_dav::AddressBookInfo {
            account_id: account.account_id.clone(),
            display_name: folder.display_name.clone(),
            href: folder.id.clone(),
        })
        .collect();

    if let Some(cache) = cache {
        let _ = cache.store_address_books(&books);
        // Drop cached folders that vanished server-side so their members read
        // as deletions (the UI receiver diffs the previous snapshot against
        // this one) instead of lingering in the People screen forever.
        let live_hrefs: HashSet<&str> = books.iter().map(|book| book.href.as_str()).collect();
        if let Ok(cached_books) = cache.load_address_books() {
            for stale in cached_books.iter().filter(|book| !live_hrefs.contains(book.href.as_str())) {
                let gone: Vec<String> = cache
                    .load_contacts()
                    .ok()
                    .map(|contacts| {
                        contacts
                            .iter()
                            .filter(|contact| contact.book_href == stale.href)
                            .map(|contact| contact.href.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                let _ = cache.apply_delta(&stale.href, None, &[], &gone);
            }
        }
    }

    for folder in &folders {
        let result = match cache {
            Some(cache) => sync_graph_folder_into(client, folder, cache).await,
            None => client.list_contacts(Some(&folder.id)).await.map(|_| ()).map_err(|e| e.to_string()),
        };
        if let Err(e) = result {
            tracing::warn!("Graph contacts sync failed for folder {:?}: {e}", folder.display_name);
        }
    }
    tracing::debug!("Graph contacts sync for {} complete ({} folder(s))", account.display_name, folders.len());

    // Full-set semantics: the per-folder syncs above applied to the cache, so
    // the snapshot is rebuilt from it (a contact missing from it was genuinely
    // deleted). Without a cache, the folders are re-read into the snapshot.
    if let Some(cache) = cache {
        if let Some(snapshot) = snapshot_from_cache(account, cache) {
            return Ok(snapshot);
        }
    }
    let mut contacts = Vec::new();
    let mut addresses = Vec::new();
    for folder in &folders {
        match client.list_contacts(Some(&folder.id)).await {
            Ok(records) => {
                for contact in &records {
                    addresses.extend(contact.card.email_addresses());
                }
                contacts.extend(records.into_iter().map(graph_contact_record));
            }
            Err(e) => tracing::warn!("Graph contacts fetch failed for folder {:?}: {e}", folder.display_name),
        }
    }
    let contact_lists = resolve_contact_lists(&contacts);
    Ok(ContactsAccountSnapshot {
        display_name: account.display_name.clone(),
        books,
        contacts,
        suggestions: dedupe_addresses(addresses, usize::MAX),
        contact_lists,
    })
}

/// One full folder sync: an incremental delta where a cursor is available,
/// else a full read. Either way the cache ends with the folder's complete set
/// (a full read diffs against what was cached to produce the deletions a delta
/// reports as tombstones), and any missing contact photos are fetched
/// best-effort.
async fn sync_graph_folder_into(client: &ContactsClient, folder: &lookout_graph::ContactFolder, cache: &ContactsCache) -> Result<(), String> {
    let token = cache.sync_token(&folder.id).map_err(|e| e.to_string())?;
    let (changed, removed, delta_link): (Vec<GraphContact>, Vec<String>, Option<String>) = match client.contacts_delta(Some(&folder.id), token.as_deref()).await {
        Ok(page) => (
            page.items.iter().filter_map(lookout_graph::contact_from_graph).collect(),
            page.items.iter().filter_map(lookout_graph::removed_id).collect(),
            page.delta_link,
        ),
        Err(e) => {
            tracing::warn!("Graph contacts delta failed for {:?}, falling back to a full read: {e}", folder.display_name);
            let previous: HashSet<String> = cache
                .load_contacts()
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|contact| contact.book_href == folder.id)
                .map(|contact| contact.href)
                .collect();
            let records = client.list_contacts(Some(&folder.id)).await.map_err(|e| e.to_string())?;
            let current: HashSet<String> = records.iter().map(|contact| contact.id.clone()).collect();
            let removed: Vec<String> = previous.difference(&current).cloned().collect();
            (records, removed, None)
        }
    };

    let records: Vec<ContactRecord> = changed.iter().map(|contact| graph_contact_record(contact.clone())).collect();
    cache.apply_delta(&folder.id, delta_link, &records, &removed).map_err(|e| e.to_string())?;
    fetch_graph_photos(client, &folder.id, cache, &changed).await;
    Ok(())
}

/// Fetches the photo of each just-changed contact that has none cached,
/// bounded per sync pass, and stores it on the card (`PHOTO`). A contact with
/// no photo is left alone; the sync passes the cap eventually cover a large
/// fresh address book, and a fetched photo is never re-requested (the cached
/// card carries it).
async fn fetch_graph_photos(client: &ContactsClient, folder_id: &str, cache: &ContactsCache, changed: &[GraphContact]) {
    if changed.is_empty() {
        return;
    }
    let with_photo: HashSet<String> = cache
        .load_contacts()
        .map(|contacts| contacts.into_iter().filter(|contact| contact.card.photo.is_some()).map(|contact| contact.href).collect())
        .unwrap_or_default();
    let mut updated = Vec::new();
    for contact in changed.iter().filter(|contact| !with_photo.contains(&contact.id)).take(MAX_GRAPH_PHOTO_FETCHES_PER_SYNC) {
        match client.contact_photo(&contact.id).await {
            Ok(Some(bytes)) => {
                let mut card = contact.card.clone();
                card.photo = Some(Photo {
                    media_type: None,
                    data: Some(bytes),
                    uri: None,
                });
                updated.push(ContactRecord {
                    href: contact.id.clone(),
                    etag: contact.etag.clone(),
                    card,
                });
            }
            Ok(None) => {}
            Err(e) => tracing::debug!("couldn't fetch the photo for contact {}: {e}", contact.id),
        }
    }
    if !updated.is_empty() {
        if let Err(e) = cache.upsert_contacts(folder_id, &updated) {
            tracing::warn!("couldn't store {} contact photo(s): {e}", updated.len());
        }
    }
}

/// A [`GraphContact`] as the shared [`ContactRecord`] (the Graph id is the
/// `href`, the `changeKey` the `etag`).
fn graph_contact_record(contact: GraphContact) -> ContactRecord {
    ContactRecord {
        href: contact.id,
        etag: contact.etag,
        card: contact.card,
    }
}

/// A Graph create: POST the card to its folder (`book_href` is the Graph
/// folder id from the editor's address-book picker; empty means the default
/// contacts folder).
async fn graph_create_contact(client: &ContactsClient, book_href: &str, card: &VCard) -> Result<(), String> {
    let folder = (!book_href.trim().is_empty()).then_some(book_href);
    client.create_contact(folder, card).await.map(|_| ()).map_err(|e| e.to_string())
}

/// A Graph update: PATCH the card to its contact id, guarded by its
/// `changeKey`.
async fn graph_update_contact(client: &ContactsClient, href: &str, etag: Option<&str>, card: &VCard) -> Result<(), String> {
    client.update_contact(href, etag, card).await.map(|_| ()).map_err(|e| e.to_string())
}

/// A Graph delete, guarded by the contact's `changeKey`.
async fn graph_delete_contact(client: &ContactsClient, href: &str, etag: Option<&str>) -> Result<(), String> {
    client.delete_contact(href, etag).await.map_err(|e| e.to_string())
}

/// Whether an account's contacts can be synced right now: a CardDAV account is
/// always usable, a Microsoft 365 account needs Graph consent (Config's "Sync
/// with Outlook"), which discovery waits for exactly as the calendar backend
/// does. Without consent the Graph client would only produce
/// `ConsentRequired` errors, so the account is left out until enabled.
pub fn contacts_account_usable(account: &GoaContactsAccount) -> bool {
    !account.is_microsoft_365() || crate::microsoft_oauth::MicrosoftOAuth::new(account.account_id.clone()).graph_consented()
}

/// The Graph contacts account for a discovered Microsoft 365 mailbox that GOA
/// reported no `Contacts` interface for (its `ms_graph` provider may omit it
/// entirely). The synthesised account reuses the mailbox's object path, so its
/// `AccountId` - and therefore its snapshot key, cache file and command
/// channel - matches every other module. `None` when GOA already reported
/// contacts, the account isn't Microsoft 365, or Graph access isn't granted.
pub fn microsoft_graph_contacts_account(account: &DiscoveredGoaAccount, account_id: &AccountId) -> Option<GoaContactsAccount> {
    let consented = crate::microsoft_oauth::MicrosoftOAuth::new(account_id.clone()).graph_consented();
    microsoft_graph_contacts_account_if_consented(account, account_id, consented)
}

/// The consent-free half of [`microsoft_graph_contacts_account`], split out so
/// the routing rule (M365 mailbox, no GOA contacts, Graph granted) is testable
/// without a consent token on disk.
fn microsoft_graph_contacts_account_if_consented(account: &DiscoveredGoaAccount, account_id: &AccountId, consented: bool) -> Option<GoaContactsAccount> {
    if account.contacts.is_some() || !consented {
        return None;
    }
    let mail = account.mail.as_ref()?;
    graph_contacts_account_from_mail(mail, account_id, &account.display_name, consented)
}

/// Synthesises the Graph contacts account for a Microsoft 365 mailbox, using
/// the mailbox's object path so its `AccountId` - and therefore its snapshot
/// key, cache file and command channel - matches every other module. `None`
/// for a non-Microsoft account or while Graph access isn't granted.
fn graph_contacts_account_from_mail(mail: &GoaMailAccount, account_id: &AccountId, display_name: &str, consented: bool) -> Option<GoaContactsAccount> {
    if !consented || !mail.is_microsoft_365() {
        return None;
    }
    Some(GoaContactsAccount {
        account_id: account_id.clone(),
        object_path: mail.object_path.clone(),
        display_name: display_name.to_string(),
        identity: mail.email.clone(),
        uri: String::new(),
        accept_ssl_errors: false,
        auth: ContactsAuthMethod::OAuth2,
        provider_type: mail.provider_type.clone(),
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
                crate::privacy::mark_ui(&hint);
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
            let summary = glib::markup_escape_text(&format!("{failures} change(s) couldn't be saved"));
            toast_overlay.add_toast(adw::Toast::new(&summary));
        } else if skipped > 0 {
            toast_overlay.add_toast(adw::Toast::new("Some changes couldn't be saved"));
        } else {
            toast_overlay.add_toast(adw::Toast::new(success));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(card: VCard) -> lookout_dav::ContactRecord {
        lookout_dav::ContactRecord {
            href: String::new(),
            etag: None,
            card,
        }
    }

    /// A re-poll that finds the same records is the same content; a changed
    /// etag, card or account name is not.
    #[test]
    fn same_content_spots_a_poll_that_changed_nothing() {
        let with_etag = |etag: &str, card: VCard| lookout_dav::ContactRecord {
            href: "/b/ann.vcf".to_string(),
            etag: Some(etag.to_string()),
            card,
        };
        let ann = person_card("ann", "ann@example.com");
        let before = test_snapshot("Work", vec![with_etag("1", ann.clone())]);
        assert!(before.same_content(&test_snapshot("Work", vec![with_etag("1", ann.clone())])));
        assert!(!before.same_content(&test_snapshot("Work", vec![with_etag("2", ann.clone())])));
        assert!(!before.same_content(&test_snapshot("Work", vec![with_etag("1", person_card("ann", "ann@example.org"))])));
        assert!(!before.same_content(&test_snapshot("Home", vec![with_etag("1", ann.clone())])));
        assert!(!before.same_content(&test_snapshot("Work", Vec::new())));
    }

    fn group_card(uid: &str, name: &str, members: &[&str]) -> VCard {
        let member_lines = members.iter().map(|m| format!("X-ADDRESSBOOKSERVER-MEMBER:{m}\r\n")).collect::<String>();
        VCard::parse(&format!(
            "BEGIN:VCARD\r\nVERSION:4.0\r\nKIND:group\r\nUID:{uid}\r\nFN:{name}\r\n{member_lines}END:VCARD\r\n"
        ))
        .unwrap()
    }

    fn person_card(uid: &str, email: &str) -> VCard {
        VCard::parse(&format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\nFN:{uid}\r\nEMAIL:{email}\r\nEND:VCARD\r\n")).unwrap()
    }

    #[test]
    fn contact_filter_matches_name_email_and_organization() {
        let card = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Jane Doe\r\nN:Doe;Jane;;;\r\nORG:Acme Corp\r\nEMAIL:jane@example.com\r\nEND:VCARD\r\n").unwrap();
        assert!(contact_matches_filter(&card, "jane"));
        assert!(contact_matches_filter(&card, "doe"));
        assert!(contact_matches_filter(&card, "example.com"));
        assert!(contact_matches_filter(&card, "acme"));
        assert!(!contact_matches_filter(&card, "spock"));
    }

    #[test]
    fn contact_letter_uppercases_and_buckets_non_alphabetic_under_hash() {
        assert_eq!(contact_letter("doe, jane"), 'D');
        assert_eq!(contact_letter("  aaron"), 'A');
        assert_eq!(contact_letter("123 company"), '#');
        assert_eq!(contact_letter("@acme"), '#');
        assert_eq!(contact_letter(""), '#');
        assert_eq!(contact_letter("  "), '#');
    }

    #[test]
    fn contacts_filter_options_narrow_by_category_and_presence() {
        let card = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Jane Doe\r\nEMAIL:jane@example.com\r\nTEL:+15551234\r\nCATEGORIES:Friends,Work\r\nEND:VCARD\r\n").unwrap();
        let bare = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Anon\r\nEND:VCARD\r\n").unwrap();

        assert!(ContactsFilterOptions::default().matches(&card));
        assert!(ContactsFilterOptions::default().matches(&bare));

        let friends = ContactsFilterOptions {
            category: Some("friends".to_string()),
            ..Default::default()
        };
        assert!(friends.matches(&card), "the category match is case-insensitive");
        assert!(!friends.matches(&bare));

        let with_email = ContactsFilterOptions {
            has_email: true,
            ..Default::default()
        };
        assert!(with_email.matches(&card));
        assert!(!with_email.matches(&bare));

        let with_phone = ContactsFilterOptions {
            has_phone: true,
            ..Default::default()
        };
        assert!(with_phone.matches(&card));
        assert!(!with_phone.matches(&bare));
    }

    #[test]
    fn resolve_contact_lists_follows_uid_members_and_mailto_members() {
        let contacts = vec![
            record(person_card("card-a", "kirk@example.com")),
            record(person_card("card-b", "spock@example.com")),
            record(group_card("crew", "Bridge Crew", &["urn:uuid:card-a", "urn:uuid:card-b", "mailto:uhura@example.com"])),
        ];
        let lists = resolve_contact_lists(&contacts);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].name, "Bridge Crew");
        let members: Vec<&str> = lists[0].members.iter().map(|m| m.address.as_str()).collect();
        assert_eq!(members, ["kirk@example.com", "spock@example.com", "uhura@example.com"]);
    }

    #[test]
    fn resolve_contact_lists_skips_unresolved_members_and_duplicates() {
        let contacts = vec![
            record(person_card("card-a", "kirk@example.com")),
            record(group_card("crew", "Bridge Crew", &["urn:uuid:card-a", "urn:uuid:missing", "mailto:kirk@example.com"])),
        ];
        let lists = resolve_contact_lists(&contacts);
        assert_eq!(lists[0].members.len(), 1);
        assert_eq!(lists[0].members[0].address, "kirk@example.com");
    }

    #[test]
    fn contact_list_prefix_matches_on_name() {
        let list = ContactList {
            name: "Bridge Crew".to_string(),
            members: Vec::new(),
        };
        assert!(contact_list_matches_prefix(&list, ""));
        assert!(contact_list_matches_prefix(&list, "brid"));
        assert!(contact_list_matches_prefix(&list, "Bridge"));
        assert!(!contact_list_matches_prefix(&list, "crew"));
    }

    fn directory_person(name: &str, address: &str) -> DirectoryPerson {
        DirectoryPerson {
            id: Some(format!("id-{address}")),
            name: Some(name.to_string()),
            address: address.to_string(),
            title: None,
            company: None,
        }
    }

    /// Flattens a merged suggestion list to the addresses it would insert, in
    /// order - the shape the ordering/dedupe assertions actually care about.
    fn merged_addresses(merged: &[crate::recipient_entry::Suggestion]) -> Vec<String> {
        merged
            .iter()
            .filter_map(|suggestion| match suggestion {
                crate::recipient_entry::Suggestion::Address(address) => Some(address.address.clone()),
                crate::recipient_entry::Suggestion::Directory(person) => Some(person.address.clone()),
                crate::recipient_entry::Suggestion::List(_) => None,
            })
            .collect()
    }

    #[test]
    fn directory_hits_merge_after_stored_contacts() {
        let merged = merge_contact_suggestions(
            Vec::new(),
            &[EmailAddress::new("ada@x.com")],
            &[directory_person("Grace Hopper", "grace@y.com")],
            &[],
            "",
            8,
        );
        assert_eq!(merged_addresses(&merged), ["ada@x.com", "grace@y.com"]);
        // The directory hit keeps its own variant (for the read-only rows of
        // 4.7.7/4.7.8), and its display name rides along.
        assert!(matches!(
            &merged[1],
            crate::recipient_entry::Suggestion::Directory(person) if person.name.as_deref() == Some("Grace Hopper")
        ));
    }

    #[test]
    fn directory_dedupes_against_mail_history_and_carddav() {
        let merged = merge_contact_suggestions(
            vec![EmailAddress::new("ada@x.com")],
            &[EmailAddress::new("bob@x.com")],
            &[
                directory_person("Ada", "ADA@x.com"),
                directory_person("Bob", "bob@x.com"),
                directory_person("Grace", "grace@y.com"),
            ],
            &[],
            "",
            8,
        );
        // A directory entry for an address already offered by mail history or
        // CardDAV is dropped (case-insensitively); only Grace is new.
        assert_eq!(merged_addresses(&merged), ["ada@x.com", "bob@x.com", "grace@y.com"]);
    }

    #[test]
    fn the_limit_bounds_stored_contacts_then_directory_then_lists() {
        let carddav: Vec<EmailAddress> = (0..8).map(|i| EmailAddress::new(format!("a{i}@x.com"))).collect();
        let merged = merge_contact_suggestions(
            Vec::new(),
            &carddav,
            &[directory_person("Grace", "grace@y.com")],
            &[ContactList {
                name: "Bridge Crew".to_string(),
                members: Vec::new(),
            }],
            "",
            8,
        );
        // Stored contacts already fill the limit, so neither the directory hit
        // nor the list can appear.
        assert_eq!(merged.len(), 8);
        assert!(merged.iter().all(|s| matches!(s, crate::recipient_entry::Suggestion::Address(_))));
    }

    #[test]
    fn directory_entries_without_an_address_are_skipped() {
        let merged = merge_contact_suggestions(Vec::new(), &[], &[directory_person("No Address", "  ")], &[], "", 8);
        assert!(merged.is_empty());
    }

    #[test]
    fn directory_matches_the_prefix_on_name_or_address() {
        let people = vec![directory_person("Grace Hopper", "grace@y.com"), directory_person("Ada Lovelace", "ada@x.com")];
        let by_name = merge_contact_suggestions(Vec::new(), &[], &people, &[], "gra", 8);
        assert_eq!(merged_addresses(&by_name), ["grace@y.com"]);
        let by_address = merge_contact_suggestions(Vec::new(), &[], &people, &[], "ada@", 8);
        assert_eq!(merged_addresses(&by_address), ["ada@x.com"]);
        assert!(merge_contact_suggestions(Vec::new(), &[], &people, &[], "zzz", 8).is_empty());
    }

    #[test]
    fn an_empty_directory_leaves_the_old_merge_result_unchanged() {
        let carddav = [EmailAddress::new("ada@x.com")];
        let lists = [ContactList {
            name: "Bridge Crew".to_string(),
            members: Vec::new(),
        }];
        let merged = merge_contact_suggestions(vec![EmailAddress::new("kirk@x.com")], &carddav, &[], &lists, "", 8);
        assert!(matches!(merged[0], crate::recipient_entry::Suggestion::Address(_)));
        assert!(matches!(merged[1], crate::recipient_entry::Suggestion::Address(_)));
        assert!(matches!(merged[2], crate::recipient_entry::Suggestion::List(_)));
    }

    #[test]
    fn contact_file_format_follows_the_extension() {
        use std::path::Path;
        assert_eq!(contact_file_format_for_path(Path::new("/tmp/contacts.csv")), ContactFileFormat::OutlookCsv);
        assert_eq!(contact_file_format_for_path(Path::new("/tmp/CONTACTS.CSV")), ContactFileFormat::OutlookCsv);
        assert_eq!(contact_file_format_for_path(Path::new("/tmp/contacts.vcf")), ContactFileFormat::VCard);
        assert_eq!(contact_file_format_for_path(Path::new("/tmp/contacts.vcard")), ContactFileFormat::VCard);
        assert_eq!(contact_file_format_for_path(Path::new("/tmp/contacts")), ContactFileFormat::VCard);
    }

    #[test]
    fn contact_cards_reads_both_formats() {
        let vcf = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Grace Hopper\r\nEND:VCARD\r\n";
        let vcards = contact_cards(ContactFileFormat::VCard, vcf);
        assert_eq!(vcards.len(), 1);
        assert_eq!(vcards[0].as_ref().unwrap().full_name.as_deref(), Some("Grace Hopper"));

        let csv = "First Name,Last Name,E-mail Address\r\nGrace,Hopper,grace@example.com\r\n";
        let csv_cards = contact_cards(ContactFileFormat::OutlookCsv, csv);
        assert_eq!(csv_cards.len(), 1);
        assert_eq!(csv_cards[0].as_ref().unwrap().emails[0].address, "grace@example.com");

        // A file with no recognisable header yields no cards for either path.
        assert!(contact_cards(ContactFileFormat::OutlookCsv, "Name,Phone\r\nGrace,1\r\n").is_empty());
        assert!(contact_cards(ContactFileFormat::VCard, "not a card").is_empty());
    }

    /// A discovered GOA account for the routing tests, with an optional Mail
    /// and Contacts interface.
    fn discovered(provider_type: Option<&str>, mail: bool, contacts: bool) -> DiscoveredGoaAccount {
        let mail_account = mail.then(|| lookout_goa::GoaMailAccount {
            account_id: AccountId("acct".to_string()),
            object_path: zbus::zvariant::OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/a").unwrap(),
            display_name: "Work".to_string(),
            email: "work@contoso.com".to_string(),
            imap: lookout_goa::EndpointConfig {
                host: "outlook.office365.com".to_string(),
                port: None,
                use_tls: true,
                accept_ssl_errors: false,
                username: "work@contoso.com".to_string(),
            },
            smtp: lookout_goa::EndpointConfig {
                host: "smtp.office365.com".to_string(),
                port: None,
                use_tls: true,
                accept_ssl_errors: false,
                username: "work@contoso.com".to_string(),
            },
            auth: lookout_goa::AuthMethod::OAuth2,
            provider_type: provider_type.map(str::to_string),
        });
        DiscoveredGoaAccount {
            display_name: "Work".to_string(),
            email: "work@contoso.com".to_string(),
            provider_type: provider_type.map(str::to_string),
            mail: mail_account,
            calendar: None,
            contacts: contacts.then(|| GoaContactsAccount {
                account_id: AccountId("acct".to_string()),
                object_path: zbus::zvariant::OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/a").unwrap(),
                display_name: "Work".to_string(),
                identity: "work@contoso.com".to_string(),
                uri: String::new(),
                accept_ssl_errors: false,
                auth: ContactsAuthMethod::OAuth2,
                provider_type: provider_type.map(str::to_string),
            }),
        }
    }

    #[test]
    fn microsoft_contacts_fallback_requires_an_m365_mailbox_without_goa_contacts() {
        let id = AccountId("acct".to_string());
        // The normal case: a consented M365 mailbox with no GOA Contacts
        // interface is synthesised against the mailbox's object path.
        let account = microsoft_graph_contacts_account_if_consented(&discovered(Some("ms_graph"), true, false), &id, true).expect("synthesised");
        assert!(account.is_microsoft_365());
        assert!(account.uri.is_empty());
        assert_eq!(account.account_id, id);
        assert_eq!(account.object_path, discovered(Some("ms_graph"), true, false).mail.unwrap().object_path);

        // Every disqualifier yields nothing.
        assert!(
            microsoft_graph_contacts_account_if_consented(&discovered(Some("ms_graph"), true, true), &id, true).is_none(),
            "GOA already reported contacts"
        );
        assert!(
            microsoft_graph_contacts_account_if_consented(&discovered(Some("ms_graph"), true, false), &id, false).is_none(),
            "Graph access is not granted"
        );
        assert!(
            microsoft_graph_contacts_account_if_consented(&discovered(Some("google"), true, false), &id, true).is_none(),
            "not Microsoft 365"
        );
        assert!(
            microsoft_graph_contacts_account_if_consented(&discovered(Some("ms_graph"), false, false), &id, true).is_none(),
            "no mailbox to hang the account off"
        );
    }

    #[test]
    fn a_non_microsoft_account_is_always_usable_without_graph_consent() {
        // A CardDAV account needs no Graph grant; only Microsoft 365 does.
        let google = discovered(Some("google"), true, true).contacts.unwrap();
        assert!(contacts_account_usable(&google));
    }

    #[test]
    fn graph_contact_record_uses_the_graph_id_and_change_key() {
        let contact = lookout_graph::GraphContact {
            id: "c1".to_string(),
            etag: Some("ck1".to_string()),
            card: person_card("c1", "ada@example.com"),
        };
        let record = graph_contact_record(contact);
        assert_eq!(record.href, "c1");
        assert_eq!(record.etag.as_deref(), Some("ck1"));
        assert_eq!(record.card.emails[0].address, "ada@example.com");
    }

    #[test]
    fn the_directory_bucket_is_a_marker_with_no_stored_members() {
        let choice = ContactsCategoryChoice {
            kind: ContactsBucketKind::Directory,
            contacts: Vec::new(),
        };
        assert!(choice.is_directory());
        assert_eq!(choice.label(), "Directory");
        assert!(choice.members().is_empty());
        // Every stored bucket stays a non-directory row.
        assert!(!ContactsCategoryChoice {
            kind: ContactsBucketKind::AllContacts,
            contacts: Vec::new(),
        }
        .is_directory());
    }

    #[test]
    fn a_directory_person_labels_falls_back_and_builds_a_seed_card() {
        let named = DirectoryPerson {
            id: Some("u1".to_string()),
            name: Some("  Grace Hopper  ".to_string()),
            address: " grace@example.com ".to_string(),
            title: Some("Admiral".to_string()),
            company: Some("US Navy".to_string()),
        };
        assert_eq!(directory_person_display_name(&named), "Grace Hopper");
        assert_eq!(directory_person_secondary(&named), "Admiral · US Navy");

        let seed = directory_person_seed(&named);
        assert_eq!(seed.full_name.as_deref(), Some("Grace Hopper"));
        assert_eq!(seed.title.as_deref(), Some("Admiral"));
        assert_eq!(seed.organization.as_deref(), Some(&["US Navy".to_string()][..]));
        assert_eq!(seed.emails.len(), 1);
        assert_eq!(seed.emails[0].address, "grace@example.com");
        assert!(seed.uid.is_some());

        // A nameless, title-less hit uses the address for both lines and
        // leaves the optional card fields empty.
        let bare = DirectoryPerson {
            id: None,
            name: None,
            address: "ada@example.com".to_string(),
            title: None,
            company: None,
        };
        assert_eq!(directory_person_display_name(&bare), "ada@example.com");
        assert_eq!(directory_person_secondary(&bare), "ada@example.com");
        let seed = directory_person_seed(&bare);
        assert_eq!(seed.full_name.as_deref(), Some("ada@example.com"));
        assert!(seed.title.is_none());
        assert!(seed.organization.is_none());
    }
}
