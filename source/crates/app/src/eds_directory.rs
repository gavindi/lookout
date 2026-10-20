//! Desktop address books as a directory (EDS_TODO.md, E1.2): a
//! [`DirectoryBackend`] over the GNOME desktop's Evolution Data Server, so
//! recipient autocomplete, the contact picker and People search also find the
//! people in GNOME Contacts' and Evolution's address books.
//!
//! Desktop address books aren't tied to a Lookout account, so one backend
//! serves every directory source, and all of them share one process-wide
//! [`LiveDesktopBooks`]: one EDS connection, opened lazily on the first lookup,
//! with each address book opened once and kept for later keystrokes. Like the
//! other directories, every failure means "no hits" (§10): no EDS on the bus,
//! an unsupported EDS, a book that won't open or a query that fails.
//!
//! Which books are searched is decided by [`eligible_books`] (E1.3): every
//! enabled address book except those belonging to a GOA account whose
//! contacts Lookout already syncs itself, so the same people don't arrive
//! twice. The whole feature follows Config → People → "Include desktop
//! address books" (`eds-address-books`, E1.4) through [`set_enabled`]; turning
//! it off takes effect on the next lookup, in composers already open too, and
//! [`disconnect_desktop`] lets go of EDS.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lookout_core::AccountId;
use lookout_eds::{people_from_card, Eds, EdsAddressBook, EdsPerson, EdsSource, EdsSourceKind};
use lookout_graph::DirectoryPerson;

use crate::directory::{merge_directory_hits, DirectoryBackend};

/// How long one address book may take to answer before the lookup goes on
/// without it. Local books answer in milliseconds and EDS answers remote
/// (CardDAV, Microsoft 365) books from its own cache, but the first query on a
/// cold book can take about a second, and an Evolution-configured LDAP book is
/// a live network round trip.
const BOOK_TIMEOUT: Duration = Duration::from_secs(3);

/// How long "EDS is unavailable" is remembered before the next lookup checks
/// again, so a desktop without EDS doesn't pay a D-Bus round trip per
/// keystroke, while an EDS that starts later is still found.
const UNAVAILABLE_RETRY: Duration = Duration::from_secs(60);

/// The `eds-address-books` setting, mirrored here so the backend - which runs
/// on the worker thread, away from GSettings - can read it on every lookup.
static DESKTOP_BOOKS_ENABLED: AtomicBool = AtomicBool::new(true);

/// Turns desktop address books on or off for every directory source. Called
/// with the stored setting at startup and whenever the Config switch changes.
pub(crate) fn set_enabled(enabled: bool) {
    DESKTOP_BOOKS_ENABLED.store(enabled, Ordering::Relaxed);
}

/// The one process-wide live desktop every [`EdsDirectoryBackend::desktop`] shares.
fn live_desktop() -> Arc<LiveDesktopBooks> {
    static DESKTOP: OnceLock<Arc<LiveDesktopBooks>> = OnceLock::new();
    DESKTOP.get_or_init(|| Arc::new(LiveDesktopBooks::default())).clone()
}

/// Closes every open desktop book and drops the EDS connection; the next
/// lookup (once the feature is on again) reconnects. Runs on the worker.
pub(crate) async fn disconnect_desktop() {
    live_desktop().disconnect().await;
}

/// Whether the desktop offers address books Lookout can read: a supported
/// EDS with its address-book factory, reachable from here (a sandbox without
/// the D-Bus permission can't). Never starts EDS. Runs on the worker.
pub(crate) async fn desktop_available() -> bool {
    match Eds::connect().await {
        Ok(Some(eds)) => eds.supports_address_books(),
        Ok(None) => false,
        Err(e) => {
            tracing::debug!("EDS directory: availability check failed: {e}");
            false
        }
    }
}

/// Where an [`EdsDirectoryBackend`] gets its people: the live desktop, or a
/// fake in tests.
#[async_trait]
pub(crate) trait DesktopBooks: Send + Sync {
    /// The people matching `query` in each [eligible](eligible_books) book,
    /// one list per book in a stable book order, each at most `limit` cards'
    /// worth. `synced_goa` holds the GOA account ids (`account_…`) whose
    /// contacts Lookout syncs itself. Empty when EDS is unavailable.
    async fn search(&self, query: &str, limit: usize, synced_goa: &HashSet<String>) -> Vec<Vec<EdsPerson>>;
}

/// The desktop's address books, as one directory.
pub(crate) struct EdsDirectoryBackend {
    books: Arc<dyn DesktopBooks>,
    /// GOA account ids whose books are skipped (see [`eligible_books`]),
    /// captured when the directory source was built.
    synced_goa: Arc<HashSet<String>>,
    /// The on/off switch, read on every lookup: [`DESKTOP_BOOKS_ENABLED`]
    /// outside tests.
    enabled: &'static AtomicBool,
}

impl EdsDirectoryBackend {
    /// The backend over the desktop's EDS, skipping the books of the GOA
    /// accounts in `synced_goa`. Cheap: every instance shares the one
    /// process-wide connection, which nothing opens until a lookup runs.
    pub(crate) fn desktop(synced_goa: HashSet<String>) -> Self {
        EdsDirectoryBackend {
            books: live_desktop(),
            synced_goa: Arc::new(synced_goa),
            enabled: &DESKTOP_BOOKS_ENABLED,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_books(books: Arc<dyn DesktopBooks>, synced_goa: HashSet<String>, enabled: &'static AtomicBool) -> Self {
        EdsDirectoryBackend {
            books,
            synced_goa: Arc::new(synced_goa),
            enabled,
        }
    }
}

#[async_trait]
impl DirectoryBackend for EdsDirectoryBackend {
    async fn search(&self, query: &str, limit: usize) -> Vec<DirectoryPerson> {
        if !self.enabled.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let per_book = self.books.search(query, limit, &self.synced_goa).await;
        merge_directory_hits(per_book.into_iter().map(|people| people.into_iter().map(directory_person).collect()).collect(), limit)
    }
}

/// An EDS hit as a directory entry. There's no Graph user id, so no
/// directory photo; a card with no name keeps `name: None`, which the
/// suggestion rows show as the bare address.
fn directory_person(person: EdsPerson) -> DirectoryPerson {
    DirectoryPerson {
        id: None,
        name: person.name,
        address: person.address,
        title: person.title,
        company: person.organization,
    }
}

/// The address books a lookup searches (E1.3), in the order their hits
/// merge (the registry's: by display name). Every enabled address book
/// except one whose GOA account is in `synced_goa`: Lookout's own CardDAV or
/// Graph sync already shows that account's people, and they'd otherwise
/// appear twice. The local "Personal" book, books made in GNOME Contacts or
/// Evolution and Evolution's LDAP books carry no GOA id, so they always
/// qualify, as do the books of GOA accounts Lookout doesn't sync contacts for.
pub(crate) fn eligible_books<'a>(sources: &'a [EdsSource], synced_goa: &HashSet<String>) -> Vec<&'a EdsSource> {
    sources
        .iter()
        .filter(|source| source.kind == EdsSourceKind::AddressBook && source.enabled)
        .filter(|source| source.goa_account_id.as_ref().is_none_or(|id| !synced_goa.contains(id)))
        .collect()
}

/// The GOA account id EDS records for a Lookout account (`account_…`, the
/// last segment of the GOA object path Lookout keys GOA accounts by), or
/// `None` for an account that doesn't come from GOA.
pub(crate) fn goa_account_id(account: &AccountId) -> Option<&str> {
    account
        .0
        .strip_prefix("/org/gnome/OnlineAccounts/Accounts/")
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

/// The live desktop: one EDS connection and the books opened on it, behind
/// an async mutex, so lookups take turns (one runs per debounce anyway).
#[derive(Default)]
struct LiveDesktopBooks {
    state: tokio::sync::Mutex<Connection>,
}

impl LiveDesktopBooks {
    async fn disconnect(&self) {
        let previous = std::mem::take(&mut *self.state.lock().await);
        if let Connection::Connected { books, .. } = previous {
            for book in books.into_values() {
                book.close().await;
            }
        }
    }
}

#[derive(Default)]
enum Connection {
    /// Nothing tried yet.
    #[default]
    Idle,
    /// EDS wasn't usable at `since`; checked again after [`UNAVAILABLE_RETRY`].
    Unavailable { since: Instant },
    /// Connected, with the books opened so far, by source UID.
    Connected { eds: Eds, books: HashMap<String, EdsAddressBook> },
}

#[async_trait]
impl DesktopBooks for LiveDesktopBooks {
    async fn search(&self, query: &str, limit: usize, synced_goa: &HashSet<String>) -> Vec<Vec<EdsPerson>> {
        let mut state = self.state.lock().await;
        if let Connection::Unavailable { since } = *state {
            if since.elapsed() < UNAVAILABLE_RETRY {
                return Vec::new();
            }
        }
        if !matches!(*state, Connection::Connected { .. }) {
            *state = match Eds::connect().await {
                Ok(Some(eds)) if eds.supports_address_books() => Connection::Connected { eds, books: HashMap::new() },
                Ok(_) => Connection::Unavailable { since: Instant::now() },
                Err(e) => {
                    tracing::debug!("EDS directory: couldn't connect: {e}");
                    Connection::Unavailable { since: Instant::now() }
                }
            };
        }
        let Connection::Connected { eds, books } = &mut *state else {
            return Vec::new();
        };

        // Re-read the source list on every lookup (one `GetManagedObjects`,
        // a few milliseconds), so a book added, removed or switched off on the
        // desktop is picked up without a restart.
        let sources = match eds.sources().await {
            Ok(sources) => sources,
            Err(e) => {
                tracing::debug!("EDS directory: couldn't list sources, treating EDS as unavailable: {e}");
                *state = Connection::Unavailable { since: Instant::now() };
                return Vec::new();
            }
        };
        let wanted = eligible_books(&sources, synced_goa);
        sync_open_books(eds, books, &wanted).await;

        let lookups = wanted.iter().filter_map(|source| books.get(&source.uid)).map(|book| async move {
            let result = tokio::time::timeout(BOOK_TIMEOUT, book.search(query, limit)).await;
            (book.source_uid().to_string(), result)
        });
        let results = futures_util::future::join_all(lookups).await;
        let mut per_book = Vec::new();
        let mut failed = Vec::new();
        for (uid, result) in results {
            match result {
                Ok(Ok(cards)) => per_book.push(cards.iter().flat_map(people_from_card).collect()),
                Ok(Err(e)) => {
                    // Forget the handle so the next lookup reopens it - EDS
                    // may have restarted or closed the backend.
                    tracing::debug!("EDS directory: search in {uid} failed: {e}");
                    failed.push(uid);
                }
                Err(_) => tracing::debug!("EDS directory: {uid} didn't answer within {BOOK_TIMEOUT:?}"),
            }
        }
        for uid in failed {
            books.remove(&uid);
        }
        per_book
    }
}

/// Opens the wanted books that aren't open yet and closes the ones no longer
/// wanted. A book that fails to open is skipped (and retried next lookup).
async fn sync_open_books(eds: &Eds, books: &mut HashMap<String, EdsAddressBook>, wanted: &[&EdsSource]) {
    let stale: Vec<String> = books.keys().filter(|uid| !wanted.iter().any(|source| &source.uid == *uid)).cloned().collect();
    for uid in stale {
        if let Some(book) = books.remove(&uid) {
            book.close().await;
        }
    }
    for source in wanted {
        if books.contains_key(&source.uid) {
            continue;
        }
        match tokio::time::timeout(BOOK_TIMEOUT, EdsAddressBook::open(eds, &source.uid)).await {
            Ok(Ok(book)) => {
                books.insert(source.uid.clone(), book);
            }
            Ok(Err(e)) => tracing::debug!("EDS directory: couldn't open {} ({}): {e}", source.display_name, source.uid),
            Err(_) => tracing::debug!("EDS directory: opening {} ({}) timed out", source.display_name, source.uid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Mutex;

    use gtk::glib;

    use crate::directory::DirectorySource;
    use crate::worker::Worker;

    fn eds_person(name: Option<&str>, address: &str) -> EdsPerson {
        EdsPerson {
            name: name.map(str::to_string),
            address: address.to_string(),
            organization: None,
            title: None,
        }
    }

    fn directory_person_named(name: &str, address: &str) -> DirectoryPerson {
        DirectoryPerson {
            id: Some(format!("id-{address}")),
            name: Some(name.to_string()),
            address: address.to_string(),
            title: None,
            company: None,
        }
    }

    /// Desktop books answering fixed hits per book, recording each query
    /// and the skipped GOA ids it was given.
    struct FakeBooks {
        per_book: Vec<Vec<EdsPerson>>,
        queries: Mutex<Vec<(String, usize, Vec<String>)>>,
    }

    impl FakeBooks {
        fn new(per_book: Vec<Vec<EdsPerson>>) -> Arc<FakeBooks> {
            Arc::new(FakeBooks {
                per_book,
                queries: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl DesktopBooks for FakeBooks {
        async fn search(&self, query: &str, limit: usize, synced_goa: &HashSet<String>) -> Vec<Vec<EdsPerson>> {
            let mut synced: Vec<String> = synced_goa.iter().cloned().collect();
            synced.sort();
            self.queries.lock().unwrap().push((query.to_string(), limit, synced));
            self.per_book.clone()
        }
    }

    /// Another directory (a GAL or LDAP server) answering fixed hits.
    struct FakeDirectory {
        hits: Vec<DirectoryPerson>,
    }

    #[async_trait]
    impl DirectoryBackend for FakeDirectory {
        async fn search(&self, _query: &str, _limit: usize) -> Vec<DirectoryPerson> {
            self.hits.clone()
        }
    }

    /// An on/off switch of the test's own, so tests never flip the
    /// process-wide one under each other.
    fn switch(on: bool) -> &'static AtomicBool {
        Box::leak(Box::new(AtomicBool::new(on)))
    }

    fn backend(books: Arc<FakeBooks>) -> EdsDirectoryBackend {
        EdsDirectoryBackend::with_books(books, HashSet::new(), switch(true))
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
    }

    #[test]
    fn hits_map_to_directory_people() {
        let person = EdsPerson {
            name: Some("Ann Example".to_string()),
            address: "ann@work.example".to_string(),
            organization: Some("Example Pty Ltd".to_string()),
            title: Some("Engineer".to_string()),
        };
        assert_eq!(
            directory_person(person),
            DirectoryPerson {
                id: None,
                name: Some("Ann Example".to_string()),
                address: "ann@work.example".to_string(),
                title: Some("Engineer".to_string()),
                company: Some("Example Pty Ltd".to_string()),
            }
        );
        assert_eq!(directory_person(eds_person(None, "bob@example.com")).name, None);
    }

    #[test]
    fn books_merge_in_order_deduplicated_case_insensitively_and_bounded() {
        let books = FakeBooks::new(vec![
            vec![eds_person(Some("Ann"), "ann@example.com"), eds_person(Some("Ann Home"), "ann@home.example")],
            vec![eds_person(Some("Ann (work book)"), "ANN@Example.com"), eds_person(None, "anna@example.net")],
        ]);
        let backend = backend(books.clone());
        let hits = block_on(backend.search("ann", 8));
        let got: Vec<(Option<&str>, &str)> = hits.iter().map(|p| (p.name.as_deref(), p.address.as_str())).collect();
        assert_eq!(got, [(Some("Ann"), "ann@example.com"), (Some("Ann Home"), "ann@home.example"), (None, "anna@example.net")]);
        assert_eq!(*books.queries.lock().unwrap(), [("ann".to_string(), 8, Vec::new())]);

        assert_eq!(block_on(backend.search("ann", 2)).len(), 2);
    }

    #[test]
    fn an_unavailable_desktop_adds_nothing() {
        let backend = backend(FakeBooks::new(Vec::new()));
        assert!(block_on(backend.search("ann", 8)).is_empty());
    }

    #[test]
    fn switched_off_asks_no_book() {
        let books = FakeBooks::new(vec![vec![eds_person(Some("Ann"), "ann@example.com")]]);
        let backend = EdsDirectoryBackend::with_books(books.clone(), HashSet::new(), switch(false));
        assert!(block_on(backend.search("ann", 8)).is_empty());
        assert!(books.queries.lock().unwrap().is_empty());
    }

    #[test]
    fn the_switch_is_read_on_every_lookup() {
        let books = FakeBooks::new(vec![vec![eds_person(Some("Ann"), "ann@example.com")]]);
        let on = switch(false);
        let backend = EdsDirectoryBackend::with_books(books, HashSet::new(), on);
        assert!(block_on(backend.search("ann", 8)).is_empty());
        on.store(true, Ordering::Relaxed);
        assert_eq!(block_on(backend.search("ann", 8)).len(), 1, "an open composer picks the change up");
    }

    #[test]
    fn synced_goa_accounts_are_handed_to_the_books() {
        let books = FakeBooks::new(Vec::new());
        let synced: HashSet<String> = ["account_2".to_string(), "account_1".to_string()].into();
        block_on(EdsDirectoryBackend::with_books(books.clone(), synced, switch(true)).search("ann", 8));
        assert_eq!(books.queries.lock().unwrap()[0].2, ["account_1", "account_2"]);
    }

    fn book(uid: &str, kind: EdsSourceKind, enabled: bool, goa: Option<&str>) -> EdsSource {
        EdsSource {
            uid: uid.to_string(),
            display_name: uid.to_string(),
            kind,
            parent_uid: None,
            enabled,
            backend: Some("local".to_string()),
            goa_account_id: goa.map(str::to_string),
        }
    }

    #[test]
    fn eligible_books_are_enabled_address_books_in_order() {
        let sources = [
            book("contacts", EdsSourceKind::AddressBook, true, None),
            book("calendar", EdsSourceKind::Calendar, true, None),
            book("off", EdsSourceKind::AddressBook, false, None),
            book("system-address-book", EdsSourceKind::AddressBook, true, None),
            book("tasks", EdsSourceKind::TaskList, true, None),
        ];
        let uids: Vec<&str> = eligible_books(&sources, &HashSet::new()).iter().map(|s| s.uid.as_str()).collect();
        assert_eq!(uids, ["contacts", "system-address-book"]);
    }

    #[test]
    fn books_of_goa_accounts_lookout_syncs_are_skipped() {
        let sources = [
            book("google-contacts", EdsSourceKind::AddressBook, true, Some("account_1676261544_5")),
            book("google-other-contacts", EdsSourceKind::AddressBook, true, Some("account_1676261544_5")),
            book("nextcloud", EdsSourceKind::AddressBook, true, Some("account_1676251667_3")),
            book("ldap-directory", EdsSourceKind::AddressBook, true, None),
            book("system-address-book", EdsSourceKind::AddressBook, true, None),
            book("off-goa", EdsSourceKind::AddressBook, false, Some("account_9")),
        ];
        let synced: HashSet<String> = ["account_1676261544_5".to_string()].into();
        let uids: Vec<&str> = eligible_books(&sources, &synced).iter().map(|s| s.uid.as_str()).collect();
        // The synced Google account's books go; a GOA account Lookout doesn't
        // sync contacts for keeps its book, and non-GOA books always stay.
        assert_eq!(uids, ["nextcloud", "ldap-directory", "system-address-book"]);
    }

    #[test]
    fn goa_account_ids_come_from_the_object_path() {
        let id = |s: &str| AccountId(s.to_string());
        assert_eq!(goa_account_id(&id("/org/gnome/OnlineAccounts/Accounts/account_1676261544_5")), Some("account_1676261544_5"));
        assert_eq!(goa_account_id(&id("other:3f2a")), None);
        assert_eq!(goa_account_id(&id("/org/gnome/OnlineAccounts/Accounts/")), None);
        assert_eq!(goa_account_id(&id("/org/gnome/OnlineAccounts/Accounts/a/b")), None);
    }

    /// Runs one `DirectorySource` lookup to completion through the real
    /// debounce, worker dispatch and merge. The debounce timer lives on the
    /// default main context, which only the thread that owns GTK may iterate.
    fn lookup(source: &DirectorySource, prefix: &str) -> Vec<DirectoryPerson> {
        let got: Rc<RefCell<Option<Vec<DirectoryPerson>>>> = Rc::new(RefCell::new(None));
        let sink = got.clone();
        source.search(prefix, Rc::new(move |hits| *sink.borrow_mut() = Some(hits)));
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.borrow().is_none() {
            assert!(Instant::now() < deadline, "the directory lookup never completed");
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        got.take().unwrap()
    }

    #[test]
    fn desktop_hits_come_after_other_directories_in_a_directory_source() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let gal = Arc::new(FakeDirectory {
            hits: vec![
                directory_person_named("Ann (GAL)", "ann@example.com"),
                directory_person_named("Andrew", "andrew@example.com"),
            ],
        });
        let desktop = Arc::new(backend(FakeBooks::new(vec![vec![
            eds_person(Some("Ann (desktop)"), "Ann@Example.com"),
            eds_person(Some("Anouk"), "anouk@example.org"),
        ]])));
        // The order `directory::directory_source` builds: other directories, then the desktop.
        let source = DirectorySource::new(vec![gal, desktop], Rc::new(Worker::new()));
        // The fakes ignore the query; it only has to pass the minimum length.
        let hits = lookup(&source, "Ann");
        let got: Vec<&str> = hits.iter().map(|p| p.name.as_deref().unwrap()).collect();
        assert_eq!(got, ["Ann (GAL)", "Andrew", "Anouk"], "the GAL's Ann wins over the desktop duplicate");
    }

    #[test]
    fn desktop_hits_come_after_stored_contacts_in_suggestions() {
        use crate::recipient_entry::Suggestion;
        use lookout_core::EmailAddress;

        let stored = vec![EmailAddress {
            name: Some("Ann (contact)".to_string()),
            address: "ann@example.com".to_string(),
        }];
        let desktop = block_on(
            backend(FakeBooks::new(vec![vec![
                eds_person(Some("Ann (desktop)"), "ANN@example.com"),
                eds_person(Some("Anouk"), "anouk@example.org"),
            ]]))
            .search("an", 8),
        );
        let merged = crate::contacts_view::merge_contact_suggestions(stored, &[], &desktop, &[], "an", 8);
        let got: Vec<String> = merged
            .iter()
            .map(|s| match s {
                Suggestion::Address(a) => format!("contact:{}", a.address),
                Suggestion::Directory(p) => format!("directory:{}", p.address),
                Suggestion::List(l) => format!("list:{}", l.name),
            })
            .collect();
        assert_eq!(got, ["contact:ann@example.com", "directory:anouk@example.org"]);
    }

    /// End to end against the session's real EDS, read only. Ignored by
    /// default (it depends on the desktop's address books); run with
    /// `cargo test -p lookout-app live_desktop_books -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_desktop_books() {
        let query = std::env::var("LOOKOUT_EDS_QUERY").unwrap_or_else(|_| "gav".to_string());
        let worker = Worker::new();
        let (tx, rx) = std::sync::mpsc::channel();
        worker.spawn(async move {
            let backend = EdsDirectoryBackend::desktop(HashSet::new());
            let first = backend.search(&query, 8).await;
            let started = Instant::now();
            let second = backend.search(&query, 8).await;
            let _ = tx.send((first, second, started.elapsed()));
        });
        let (first, second, warm) = rx.recv_timeout(Duration::from_secs(30)).unwrap();
        println!(
            "{} hits ({} named); warm lookup took {warm:?}",
            first.len(),
            first.iter().filter(|p| p.name.is_some()).count()
        );
        assert_eq!(first, second, "a repeated lookup answers the same");
    }
}
