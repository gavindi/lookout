//! Directory lookup (OUTLOOK_PARITY.md item 4.7 / P-11): one shared async
//! source that fans a recipient/People search out across every configured
//! directory backend - the Microsoft 365 GAL (`/users?$search=`, backed by
//! `lookout_graph::contacts`) and LDAP servers - and hands back the hits the
//! recipient autocomplete, the address-book picker and People search merge in
//! beside stored contacts and mail history.
//!
//! Autocomplete fires on every keystroke, and a directory is a network round
//! trip, so a lookup is gated three ways: a minimum query length, a short
//! debounce that collapses a burst of keystrokes into one request, and a small
//! prefix→results cache so a prefix typed, deleted and retyped costs nothing.
//! A backend that is unreachable or unauthorised contributes no hits rather
//! than an error (§10, "degrade softly"); [`DirectoryBackend::search`] returns
//! a plain `Vec`, never a `Result`.
//!
//! [`directory_source`] assembles the running backends for a suggestion
//! surface: the Graph provider for each connected Microsoft 365 account whose
//! directory search the user has opted into (and whose Graph consent is in
//! place), one LDAP provider per enabled `AppConfig::ldap_directories` entry
//! in scope, and last the desktop's own address books over Evolution Data
//! Server (`eds_directory`, EDS_TODO.md E1.2). `Some(id)` scopes the source to
//! that account (the composer's To/Cc/Bcc fields, sending from `id`); `None`
//! unions every account's directories (the calendar attendee field, where an
//! invitee isn't tied to the event's calendar account). The desktop's books
//! serve both scopes. A disabled, unconsented or unconfigured source simply
//! contributes no hits.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gtk::glib;
use lookout_core::AccountId;
use lookout_graph::DirectoryPerson;

use crate::app_config::{AppConfig, LdapDirectory};
use crate::eds_directory::EdsDirectoryBackend;
use crate::graph_directory::GraphDirectoryBackend;
use crate::ldap_directory::LdapDirectoryBackend;
use crate::other_accounts::SecretServiceKeyring;
use crate::recipient_entry::Suggestion;
use crate::window::UiState;
use crate::worker::Worker;

/// The shortest query sent to a directory backend. Below this the source
/// answers empty without touching the network: a one- or two-letter prefix
/// would match most of a company directory and cost a request per keystroke
/// while the user is still typing the name.
pub(crate) const DIRECTORY_MIN_CHARS: usize = 3;

/// How long keystrokes settle before a query is dispatched. Short enough that
/// results still feel live, long enough that typing a name issues one request
/// rather than one per letter.
pub(crate) const DIRECTORY_DEBOUNCE_MS: u64 = 250;

/// The most hits a lookup returns, and the most any one backend is asked for.
/// Matches the recipient popover's own cap so a directory can't push stored
/// contacts out of view.
pub(crate) const DIRECTORY_LIMIT: usize = 8;

/// The most prefixes kept in the prefix→results cache.
pub(crate) const DIRECTORY_CACHE_CAP: usize = 32;

/// One directory provider for one account (a Microsoft 365 GAL, or an LDAP
/// server). Implementations return whatever hits they have and treat every
/// failure as "no hits": a denied scope, a dead host or a malformed reply
/// yields an empty list, never an error the UI has to surface.
#[async_trait]
pub trait DirectoryBackend: Send + Sync {
    /// Directory entries matching `query` (already trimmed, lower-cased and
    /// long enough per [`directory_query`]), at most `limit` of them.
    async fn search(&self, query: &str, limit: usize) -> Vec<DirectoryPerson>;
}

/// A prefix → directory-suggestions lookup (item 4.7.7): a [`DirectorySource`]
/// wrapped so its hits arrive ready to render as read-only
/// [`Suggestion::Directory`] rows. The recipient picker drives this directly;
/// the composer and attendee fields use the raw `search` instead, because they
/// merge directory hits *after* stored contacts rather than showing them on
/// their own.
pub(crate) type DirectoryLookup = Rc<dyn Fn(&str, Rc<dyn Fn(Vec<Suggestion>)>)>;

/// The normalised query for `prefix`, or `None` when it is too short to send.
/// Trims and lower-cases so the cache key and the backend query agree: a
/// retyped prefix with different capitalisation reuses the cached hits and
/// never issues a second request.
fn directory_query(prefix: &str) -> Option<String> {
    let trimmed = prefix.trim();
    if trimmed.chars().count() < DIRECTORY_MIN_CHARS {
        return None;
    }
    Some(trimmed.to_lowercase())
}

/// Flattens each backend's hits into one list, de-duplicated on the bare
/// address (case-insensitively) and bounded by `limit`. Backends are visited
/// in order, so an entry from the first directory that has it wins.
pub(crate) fn merge_directory_hits(per_backend: Vec<Vec<DirectoryPerson>>, limit: usize) -> Vec<DirectoryPerson> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for hits in per_backend {
        for person in hits {
            if out.len() >= limit {
                return out;
            }
            let key = person.address.trim().to_lowercase();
            if key.is_empty() || !seen.insert(key) {
                continue;
            }
            out.push(person);
        }
    }
    out
}

/// A tiny insertion-ordered prefix→results cache. Evicts the oldest prefix once
/// [`DIRECTORY_CACHE_CAP`] is reached - the working set is the handful of
/// prefixes a user types before picking someone, so recency is all the policy
/// this needs.
#[derive(Default)]
struct PrefixCache {
    entries: HashMap<String, Vec<DirectoryPerson>>,
    order: VecDeque<String>,
}

impl PrefixCache {
    fn get(&self, prefix: &str) -> Option<Vec<DirectoryPerson>> {
        self.entries.get(prefix).cloned()
    }

    fn put(&mut self, prefix: String, hits: Vec<DirectoryPerson>) {
        if self.entries.contains_key(&prefix) {
            return;
        }
        while self.entries.len() >= DIRECTORY_CACHE_CAP {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
        self.order.push_back(prefix.clone());
        self.entries.insert(prefix, hits);
    }
}

/// A pending keystroke waiting out the debounce: the normalised query it will
/// dispatch and the callback its results (or a newer request's, whichever
/// generation wins) are delivered to.
struct Pending {
    query: String,
    generation: u64,
    complete: Rc<dyn Fn(Vec<DirectoryPerson>)>,
}

/// The shared directory dispatcher. Cheap to clone; clones share the same
/// backends, cache and debounce state. One instance serves a given suggestion
/// surface (the composer's To/Cc/Bcc fields together, or the event editor's
/// attendee field); the cache is per-instance, so closing a composer drops its
/// prefixes with it.
#[derive(Clone)]
pub(crate) struct DirectorySource {
    backends: Vec<Arc<dyn DirectoryBackend>>,
    worker: Rc<Worker>,
    cache: Rc<RefCell<PrefixCache>>,
    /// The latest keystroke queued for the debounce, replaced on every call so
    /// only the newest prefix is ever dispatched.
    pending: Rc<RefCell<Option<Pending>>>,
    /// The armed debounce timer, so a new keystroke can cancel and re-arm it.
    timer: Rc<Cell<Option<glib::SourceId>>>,
    /// Bumped on every dispatched request; a reply whose generation is no
    /// longer current is dropped, the same staleness guard `RecipientEntry`
    /// applies to suggestion sources.
    generation: Rc<Cell<u64>>,
}

impl DirectorySource {
    /// Builds a source over the given per-account backends. An empty list is
    /// valid and simply answers empty (an account with no directory
    /// configured), so callers never have to branch.
    pub(crate) fn new(backends: Vec<Arc<dyn DirectoryBackend>>, worker: Rc<Worker>) -> Self {
        DirectorySource {
            backends,
            worker,
            cache: Rc::new(RefCell::new(PrefixCache::default())),
            pending: Rc::new(RefCell::new(None)),
            timer: Rc::new(Cell::new(None)),
            generation: Rc::new(Cell::new(0)),
        }
    }

    /// Looks `prefix` up and hands the hits to `complete`. Deliberately
    /// multi-shot - the caller may invoke `complete` more than once for one
    /// prefix (an immediate local answer plus the directory's, item 4.7.6) -
    /// and asynchronous: a cached prefix answers on this call, anything else
    /// after the debounce. A prefix below [`DIRECTORY_MIN_CHARS`] answers empty
    /// without querying.
    pub(crate) fn search(&self, prefix: &str, complete: Rc<dyn Fn(Vec<DirectoryPerson>)>) {
        let Some(query) = directory_query(prefix) else {
            complete(Vec::new());
            return;
        };
        if let Some(cached) = self.cache.borrow().get(&query) {
            complete(cached);
            return;
        }
        if self.backends.is_empty() {
            complete(Vec::new());
            return;
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        *self.pending.borrow_mut() = Some(Pending { query, generation, complete });
        if let Some(id) = self.timer.take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local_once(Duration::from_millis(DIRECTORY_DEBOUNCE_MS), move || {
            this.timer.set(None);
            this.dispatch();
        });
        self.timer.set(Some(id));
    }

    /// Wraps this source as a [`DirectoryLookup`] for the recipient picker
    /// (item 4.7.7): every hit becomes a [`Suggestion::Directory`] row. The
    /// closure shares this source, so the picker and the composer autocomplete
    /// it was built beside reuse one prefix cache.
    pub(crate) fn suggestion_lookup(&self) -> DirectoryLookup {
        let this = self.clone();
        Rc::new(move |prefix, complete| {
            this.search(
                prefix,
                Rc::new(move |people| {
                    complete(directory_suggestions(people));
                }),
            );
        })
    }

    /// Fires the pending query against every backend concurrently on the
    /// `Worker`, then merges, caches and delivers the result on the GTK thread.
    fn dispatch(&self) {
        let Some(pending) = self.pending.borrow_mut().take() else {
            return;
        };
        let generation = pending.generation;
        let query = pending.query;
        let complete = pending.complete;
        let backends = self.backends.clone();
        let (tx, rx) = async_channel::bounded(1);
        {
            let query = query.clone();
            self.worker.spawn(async move {
                let futures = backends.iter().map(|backend| backend.search(&query, DIRECTORY_LIMIT));
                let per_backend: Vec<Vec<DirectoryPerson>> = futures_util::future::join_all(futures).await;
                let _ = tx.send(per_backend).await;
            });
        }
        let this = self.clone();
        let cache_key = query;
        glib::spawn_future_local(async move {
            let per_backend = rx.recv().await.unwrap_or_default();
            // A keystroke since this dispatch replaced the pending request; its
            // own lookup will deliver, so this reply is stale.
            if this.generation.get() != generation {
                return;
            }
            let hits = merge_directory_hits(per_backend, DIRECTORY_LIMIT);
            this.cache.borrow_mut().put(cache_key, hits.clone());
            complete(hits);
        });
    }
}

/// Builds the directory source for one suggestion surface (item 4.7.6).
///
/// `account` scopes the source: `Some(id)` serves only that account's
/// directories (the composer's To/Cc/Bcc fields, sending from `id`), while
/// `None` unions every account's (the calendar attendee field, where an
/// invitee isn't tied to the event's calendar account). A Microsoft 365
/// account contributes its Graph GAL only when the user has opted it in and
/// its directory consent is in place ([`GraphDirectoryBackend::enabled`]); an
/// LDAP directory contributes only when it is enabled and in scope. The
/// desktop's address books ([`EdsDirectoryBackend`]) serve every scope and
/// come last; without EDS they simply answer empty.
pub(crate) fn directory_source(state: &Rc<RefCell<UiState>>, account: Option<AccountId>, worker: Rc<Worker>) -> DirectorySource {
    // Read the config and the Graph-capable account ids out under one short
    // borrow. An account carries the Graph clients only when it is a
    // Microsoft 365 account with its own token (a shared mailbox borrows its
    // parent's and is skipped here - the parent is the one with the GAL).
    let (config, graph_accounts) = {
        let st = state.borrow();
        let config = st.app_config.borrow().clone();
        let ids: Vec<AccountId> = st.accounts.iter().filter(|(_, handle)| handle.graph_pin.is_some()).map(|(id, _)| id.clone()).collect();
        (config, ids)
    };
    let mut backends: Vec<Arc<dyn DirectoryBackend>> = Vec::new();
    for id in graph_accounts {
        if account.as_ref().is_some_and(|scope| scope != &id) {
            continue;
        }
        let backend = GraphDirectoryBackend::new(id);
        if backend.enabled(&config) {
            backends.push(Arc::new(backend));
        }
    }
    // One keyring handle shared by every LDAP backend; each search opens its
    // own D-Bus connection from it, so a rotated secret is picked up without a
    // rebuild.
    let keyring = SecretServiceKeyring::new();
    for directory in ldap_directories_in_scope(&config, account.as_ref()) {
        let backend = LdapDirectoryBackend::new(directory, keyring.clone());
        if backend.enabled() {
            backends.push(Arc::new(backend));
        }
    }
    // The desktop's address books (EDS_TODO.md E1.2) go last, so a person
    // another directory also lists keeps that directory's entry. They aren't
    // tied to an account, so the same backend serves both scopes; it connects
    // to EDS on the first lookup and contributes nothing without it.
    backends.push(Arc::new(EdsDirectoryBackend::desktop()));
    DirectorySource::new(backends, worker)
}

/// The configured LDAP directories a suggestion surface in `account` scope
/// searches: `Some(id)` limits to directories serving that account, `None`
/// takes every one, both filtered to the enabled entries only. Pure, so the
/// scope rules are unit-tested without a live server or keyring.
fn ldap_directories_in_scope<'a>(config: &'a AppConfig, account: Option<&AccountId>) -> Vec<&'a LdapDirectory> {
    config
        .ldap_directories
        .iter()
        .filter(|directory| directory.enabled)
        .filter(|directory| account.is_none_or(|scope| &directory.account_id == scope))
        .collect()
}

/// Maps a source's hits to the read-only picker suggestions of item 4.7.7.
/// Pure, so the mapping is tested without a live directory.
fn directory_suggestions(people: Vec<DirectoryPerson>) -> Vec<Suggestion> {
    people.into_iter().map(Suggestion::Directory).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn person(name: &str, address: &str) -> DirectoryPerson {
        DirectoryPerson {
            id: Some(format!("id-{address}")),
            name: Some(name.to_string()),
            address: address.to_string(),
            title: None,
            company: None,
        }
    }

    struct FakeBackend {
        hits: Vec<DirectoryPerson>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl DirectoryBackend for FakeBackend {
        async fn search(&self, query: &str, _limit: usize) -> Vec<DirectoryPerson> {
            self.calls.lock().unwrap().push(query.to_string());
            self.hits.clone()
        }
    }

    #[test]
    fn directory_query_gates_on_length_and_normalises_case() {
        assert_eq!(directory_query("ab"), None);
        assert_eq!(directory_query("  a  "), None);
        assert_eq!(directory_query(""), None);
        assert_eq!(directory_query("abc"), Some("abc".to_string()));
        assert_eq!(directory_query("  Grace  "), Some("grace".to_string()));
    }

    #[test]
    fn merge_directory_hits_dedupes_case_insensitively_and_bounds() {
        let merged = merge_directory_hits(
            vec![
                vec![person("Ada", "ada@x.com"), person("Bob", "bob@x.com")],
                vec![person("Ada again", "ADA@x.com"), person("Cleo", "cleo@x.com")],
            ],
            DIRECTORY_LIMIT,
        );
        let addresses: Vec<String> = merged.iter().map(|p| p.address.clone()).collect();
        // The second directory's duplicate of Ada is dropped; Cleo is new.
        assert_eq!(addresses, ["ada@x.com", "bob@x.com", "cleo@x.com"]);

        let bounded = merge_directory_hits(vec![vec![person("Ada", "ada@x.com"), person("Bob", "bob@x.com")]], 1);
        assert_eq!(bounded.len(), 1);
        assert_eq!(bounded[0].address, "ada@x.com");
    }

    #[test]
    fn merge_directory_hits_skips_blank_addresses() {
        let merged = merge_directory_hits(vec![vec![person("No Address", "   "), person("Ada", "ada@x.com")]], DIRECTORY_LIMIT);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].address, "ada@x.com");
    }

    #[test]
    fn prefix_cache_evicts_the_oldest_past_the_cap() {
        let mut cache = PrefixCache::default();
        for i in 0..DIRECTORY_CACHE_CAP {
            cache.put(format!("p{i}"), vec![person(&format!("P{i}"), &format!("p{i}@x.com"))]);
        }
        assert!(cache.get("p0").is_some());
        cache.put("overflow".to_string(), vec![person("Over", "over@x.com")]);
        // The oldest prefix made room for the new one; the rest survive.
        assert!(cache.get("p0").is_none());
        assert!(cache.get("p1").is_some());
        assert!(cache.get("overflow").is_some());
        // Re-putting an existing prefix is a no-op, not a duplicate eviction.
        let before = cache.entries.len();
        cache.put("p1".to_string(), Vec::new());
        assert_eq!(cache.entries.len(), before);
    }

    #[test]
    fn a_short_prefix_answers_empty_without_querying_a_backend() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(FakeBackend {
            hits: vec![person("Grace", "grace@y.com")],
            calls: calls.clone(),
        });
        let source = DirectorySource::new(vec![backend], Rc::new(Worker::new()));
        let got = Rc::new(RefCell::new(vec![person("sentinel", "sentinel@x.com")]));
        let got_for_cb = got.clone();
        source.search("ab", Rc::new(move |hits| *got_for_cb.borrow_mut() = hits));
        assert!(got.borrow().is_empty());
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn no_backends_answers_empty_without_arming_a_debounce() {
        let source = DirectorySource::new(Vec::new(), Rc::new(Worker::new()));
        let got = Rc::new(RefCell::new(None));
        let got_for_cb = got.clone();
        source.search("grace", Rc::new(move |hits| *got_for_cb.borrow_mut() = Some(hits)));
        assert_eq!(got.borrow().as_deref(), Some(&[][..]));
    }

    fn ldap(id: &str, account: &str, enabled: bool) -> LdapDirectory {
        LdapDirectory {
            id: id.to_string(),
            account_id: AccountId(account.to_string()),
            display_name: id.to_string(),
            host: "ldap.example.com".to_string(),
            port: 389,
            tls: false,
            bind_dn: String::new(),
            base_dn: "dc=example,dc=com".to_string(),
            filter: String::new(),
            mail_attr: String::new(),
            name_attr: String::new(),
            enabled,
        }
    }

    #[test]
    fn ldap_directories_in_scope_filters_by_enabled_and_account() {
        let config = AppConfig {
            ldap_directories: vec![ldap("ldap:a", "account_1", true), ldap("ldap:b", "account_2", true), ldap("ldap:c", "account_1", false)],
            ..AppConfig::default()
        };

        // Account-scoped (the composer): only that account's enabled entries.
        let scoped = ldap_directories_in_scope(&config, Some(&AccountId("account_1".into())));
        assert_eq!(scoped.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["ldap:a"]);

        // Unscoped (the attendee field): every enabled entry, disabled dropped.
        let unioned = ldap_directories_in_scope(&config, None);
        assert_eq!(unioned.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["ldap:a", "ldap:b"]);
    }

    #[test]
    fn directory_suggestions_wrap_hits_as_read_only_directory_rows() {
        let suggestions = directory_suggestions(vec![person("Grace", "grace@y.com")]);
        assert_eq!(suggestions, vec![Suggestion::Directory(person("Grace", "grace@y.com"))]);
        // An empty hit list is a valid answer that clears the picker's section.
        assert!(directory_suggestions(Vec::new()).is_empty());
    }
}
