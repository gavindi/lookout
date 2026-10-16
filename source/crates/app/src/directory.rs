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
//! The Graph and LDAP backends landed in 4.7.3/4.7.5 and the composer/attendee
//! wiring follows in 4.7.6, so every item here is unused by the running app
//! until then. The `allow(dead_code)` below is removed when 4.7.6 wires it up;
//! the pure pieces ([`directory_query`], [`merge_directory_hits`],
//! [`PrefixCache`]) and the debounce gate are unit-tested now so 4.7.6 only
//! adds plumbing.

#![allow(dead_code)]

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gtk::glib;
use lookout_graph::DirectoryPerson;

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
fn merge_directory_hits(per_backend: Vec<Vec<DirectoryPerson>>, limit: usize) -> Vec<DirectoryPerson> {
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
}
