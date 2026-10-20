//! The desktop's own address books in the People screen (EDS_TODO.md, E1.5):
//! the eligible Evolution Data Server books (the same ones directory lookups
//! search, see `eds_directory::eligible_books`) listed read-only under a
//! "Desktop" sidebar section.
//!
//! Lookout never writes to them. Their entries carry an empty `href` - the
//! People screen's existing "display only" marker, shared with the Deleted
//! bucket - so the editor, Delete and the ribbon's Edit stay off without any
//! extra checks, and their account id is `eds:<source uid>` so they're never
//! mistaken for an account's. Where a writable contact offers "Edit", a desktop
//! contact offers "Open in GNOME Contacts" ([`open_in_gnome_contacts`]).
//!
//! [`reload`] refills `UiState::desktop_contacts` on the worker and repaints
//! the People screen. The window calls it after contacts discovery, when the
//! People screen is shown, every [`RELOAD_INTERVAL`] and when Config → People
//! → "Include desktop address books" changes.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::{AccountId, VCard};

use crate::contacts_view::ContactsListEntry;
use crate::eds_directory::DesktopBook;
use crate::window::UiState;
use crate::worker::Worker;

/// The account-id prefix of desktop entries: `eds:<source uid>`. A GOA
/// account id is an object path and a manual one is `other:<uuid>`, so the
/// two never collide.
const DESKTOP_ACCOUNT_PREFIX: &str = "eds:";

/// How often the Desktop section re-reads its books while Lookout runs, on
/// top of the reloads when the People screen is shown - the same cadence as
/// the CardDAV contact poll.
pub(crate) const RELOAD_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// GNOME Contacts' bus name; it's D-Bus activatable, which is how a sandboxed
/// Lookout can still bring it up.
const GNOME_CONTACTS_BUS_NAME: &str = "org.gnome.Contacts";

/// The account id a desktop book's entries carry.
pub(crate) fn desktop_account_id(source_uid: &str) -> AccountId {
    AccountId(format!("{DESKTOP_ACCOUNT_PREFIX}{source_uid}"))
}

/// Whether `entry` comes from a desktop address book (read-only in Lookout).
pub(crate) fn is_desktop_entry(entry: &ContactsListEntry) -> bool {
    entry.account_id.0.starts_with(DESKTOP_ACCOUNT_PREFIX)
}

/// One People-screen row's data: `(account, account label, record)`.
pub(crate) type DesktopRecord = (AccountId, String, lookout_dav::ContactRecord);

/// A desktop book's cards as the People screen's rows. The empty `href`
/// marks them display-only.
pub(crate) fn desktop_records(book: &DesktopBook) -> Vec<DesktopRecord> {
    let account_id = desktop_account_id(&book.uid);
    book.cards
        .iter()
        .map(|card| {
            (
                account_id.clone(),
                book.display_name.clone(),
                lookout_dav::ContactRecord {
                    href: String::new(),
                    etag: None,
                    card: card.clone(),
                },
            )
        })
        .collect()
}

thread_local! {
    /// Bumped on every [`reload`], so a slow listing that finishes after a
    /// newer one (or after the feature was switched off) is dropped.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
}

/// Re-reads the desktop's books on the worker, then stores them and repaints
/// the People screen. With the feature off the section is cleared at once.
pub(crate) fn reload(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>) {
    let state = state.clone();
    let worker = worker.clone();
    glib::spawn_future_local(async move {
        reload_now(&state, &worker).await;
    });
}

/// [`reload`], awaited: resolves once the books are stored, with whether the
/// Desktop section has any. A reload overtaken by a newer one stores nothing
/// and reports what's stored.
pub(crate) async fn reload_now(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>) -> bool {
    let generation = GENERATION.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });
    if !crate::eds_directory::enabled() {
        store(state, Vec::new());
        return false;
    }
    let synced_goa = crate::eds_directory::synced_goa_accounts(&state.borrow());
    let (tx, rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let _ = tx.send(crate::eds_directory::desktop_books_snapshot(synced_goa).await).await;
    });
    if let Ok(books) = rx.recv().await {
        if GENERATION.with(Cell::get) == generation {
            store(state, books.unwrap_or_default());
        }
    }
    !state.borrow().desktop_contacts.is_empty()
}

/// Replaces the stored books and repaints the People screen, unless nothing
/// changed (the periodic reload usually finds the same books).
fn store(state: &Rc<RefCell<UiState>>, books: Vec<DesktopBook>) {
    if state.borrow().desktop_contacts == books {
        return;
    }
    state.borrow_mut().desktop_contacts = books;
    let refresh = state.borrow().contacts_refresh.clone();
    if let Some(refresh) = refresh {
        refresh();
    }
}

/// The `gnome-contacts` command-line arguments that bring up `card`: its
/// first email address (`--email`, which selects the contact), else a search
/// for its name, else nothing (just opens the app). GNOME Contacts can't be
/// pointed at an EDS UID - its own `show-contact` action takes a folks
/// individual id - so the address is the best handle there is.
pub(crate) fn gnome_contacts_args(card: &VCard) -> Vec<String> {
    // The card's own fields, not `vcard_primary_email`/`vcard_display_name`,
    // whose placeholders ("No email address", "(unnamed contact)") would be
    // searched for literally.
    if let Some(email) = card.emails.iter().map(|email| email.address.trim()).find(|address| !address.is_empty()) {
        return vec!["--email".to_string(), email.to_string()];
    }
    let full_name = card.full_name.as_deref().map(str::trim).filter(|name| !name.is_empty()).map(str::to_string);
    let structured = card.name.as_ref().and_then(|name| {
        let parts: Vec<&str> = [&name.given, &name.family].into_iter().map(|part| part.trim()).filter(|part| !part.is_empty()).collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    });
    match full_name.or(structured) {
        Some(name) => vec!["--search".to_string(), name],
        None => Vec::new(),
    }
}

/// Opens `entry`'s contact in GNOME Contacts, where desktop contacts are
/// edited. Runs the `gnome-contacts` command when it's installed alongside
/// Lookout, which selects the contact; otherwise (GNOME Contacts installed
/// as a Flatpak, or Lookout itself sandboxed) D-Bus-activates the app, which
/// opens it without a selection. Toasts when neither works.
pub(crate) fn open_in_gnome_contacts(entry: &ContactsListEntry, toast_overlay: &adw::ToastOverlay) {
    if let Some(program) = glib::find_program_in_path("gnome-contacts") {
        let mut argv: Vec<std::ffi::OsString> = vec![program.into_os_string()];
        argv.extend(gnome_contacts_args(&entry.card).into_iter().map(Into::into));
        let argv_refs: Vec<&std::ffi::OsStr> = argv.iter().map(|arg| arg.as_os_str()).collect();
        match gio::Subprocess::newv(&argv_refs, gio::SubprocessFlags::NONE) {
            Ok(process) => {
                // Reap it; the command hands off to the running instance and exits.
                process.wait_async(None::<&gio::Cancellable>, |_| {});
                return;
            }
            Err(e) => tracing::debug!("couldn't run gnome-contacts: {e}"),
        }
    }
    let toast_overlay = toast_overlay.clone();
    glib::spawn_future_local(async move {
        let activated = async {
            let connection = gio::bus_get_future(gio::BusType::Session).await?;
            connection
                .call_future(
                    Some(GNOME_CONTACTS_BUS_NAME),
                    "/org/gnome/Contacts",
                    "org.freedesktop.Application",
                    "Activate",
                    Some(&(glib::VariantDict::new(None).end(),).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    10_000,
                )
                .await
        }
        .await;
        if let Err(e) = activated {
            tracing::debug!("couldn't activate GNOME Contacts: {e}");
            toast_overlay.add_toast(adw::Toast::new(
                "GNOME Contacts isn't installed. Edit this contact in the app that manages the address book.",
            ));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(text: &str) -> VCard {
        VCard::parse(text).unwrap()
    }

    fn entry(account: &str) -> ContactsListEntry {
        ContactsListEntry {
            account_id: AccountId(account.to_string()),
            account_label: "Personal".to_string(),
            category_label: "Personal".to_string(),
            card: card("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ann\r\nEND:VCARD\r\n"),
            href: String::new(),
            etag: None,
        }
    }

    #[test]
    fn desktop_entries_are_told_apart_by_their_account_id() {
        assert_eq!(desktop_account_id("system-address-book"), AccountId("eds:system-address-book".to_string()));
        assert!(is_desktop_entry(&entry("eds:system-address-book")));
        assert!(!is_desktop_entry(&entry("/org/gnome/OnlineAccounts/Accounts/account_1")));
        assert!(!is_desktop_entry(&entry("other:3f2a")));
    }

    #[test]
    fn desktop_records_are_display_only_and_keep_book_order() {
        let book = DesktopBook {
            uid: "143ac57f".to_string(),
            display_name: "Galaxy Users".to_string(),
            cards: vec![
                card("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Zed\r\nEND:VCARD\r\n"),
                card("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ann\r\nEMAIL:ann@example.com\r\nEND:VCARD\r\n"),
            ],
        };
        let records = desktop_records(&book);
        assert_eq!(records.len(), 2);
        for (account, label, record) in &records {
            assert_eq!(account, &AccountId("eds:143ac57f".to_string()));
            assert_eq!(label, "Galaxy Users");
            assert!(record.href.is_empty(), "an empty href keeps the editor and Delete off");
            assert!(record.etag.is_none());
        }
        assert_eq!(records[0].2.card.full_name.as_deref(), Some("Zed"));
    }

    #[test]
    fn gnome_contacts_is_pointed_at_the_address_then_the_name() {
        let with_email = card("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ann\r\nEMAIL:ann@example.com\r\nEMAIL:ann@home.example\r\nEND:VCARD\r\n");
        assert_eq!(gnome_contacts_args(&with_email), ["--email", "ann@example.com"]);

        let name_only = card("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Phone Only\r\nTEL:+61 7 5555 0100\r\nEND:VCARD\r\n");
        assert_eq!(gnome_contacts_args(&name_only), ["--search", "Phone Only"]);

        let structured = card("BEGIN:VCARD\r\nVERSION:3.0\r\nN:Zola;Émile;;;\r\nEND:VCARD\r\n");
        assert_eq!(gnome_contacts_args(&structured), ["--search", "Émile Zola"]);

        let nothing = card("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:x\r\nEND:VCARD\r\n");
        assert!(gnome_contacts_args(&nothing).is_empty(), "no placeholder text is searched for");
    }
}
