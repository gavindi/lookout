//! The LDAP directory provider for recipient autocomplete and People search
//! (OUTLOOK_PARITY.md item 4.7.5 / P-11).
//!
//! One [`DirectoryBackend`] per configured [`LdapDirectory`]
//! (`AppConfig::ldap_directories`, item 4.7.4). Each search opens a fresh
//! connection, encrypts it as configured (implicit TLS on an LDAPS port,
//! StartTLS on any other port with `tls` set), binds - anonymously when
//! `bind_dn` is empty, otherwise with the password from the keyring's
//! `PROTOCOL_LDAP` slot - runs one subtree search under `base_dn`, and maps
//! the entries to [`DirectoryPerson`]. Every failure - an unreachable host, a
//! refused bind, a missing keyring secret, a timeout - degrades to no hits,
//! never an error (§10, "degrade softly").
//!
//! The ldap3 connection sits behind the private [`DirectorySearch`] trait, so
//! the backend's plan-building, mapping and degradation are unit-tested
//! against a fake with no live server, while the filter builder (RFC 4515
//! escaping) and entry mapping are pure functions tested directly.
//!
//! The backend is unused by the running app until the composer/attendee wiring
//! lands in 4.7.6, hence the module-level `allow(dead_code)` - the same guard
//! `directory.rs` and `graph_directory.rs` carry until then.

#![allow(dead_code)]

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ldap3::{LdapConnAsync, LdapConnSettings, Scope, SearchEntry, SearchOptions};
use lookout_core::AccountId;
use lookout_graph::DirectoryPerson;

use crate::app_config::LdapDirectory;
use crate::directory::DirectoryBackend;
use crate::other_accounts::{KeyringStore, PROTOCOL_LDAP};

/// How long the connect and each bind/search operation may take before the
/// lookup is abandoned and the directory contributes no hits. The shared
/// dispatcher waits for every backend, so one unresponsive host must not hold
/// the merged autocomplete result for longer than this.
const LDAP_TIMEOUT: Duration = Duration::from_secs(8);

/// The attribute holding an entry's address when the config leaves
/// `mail_attr` empty.
const DEFAULT_MAIL_ATTR: &str = "mail";

/// The attribute holding an entry's display name when the config leaves
/// `name_attr` empty.
const DEFAULT_NAME_ATTR: &str = "displayName";

/// The ports that get an implicit TLS (`ldaps://`) connection when `tls` is
/// set: LDAP's standard LDAPS port and Active Directory's global catalog.
/// Any other port uses StartTLS.
const LDAPS_PORTS: [u16; 2] = [636, 3269];

/// One configured LDAP directory's provider. Cheap to construct - it clones
/// the config and shares the keyring; the bind password is fetched per search,
/// so a rotated or missing secret is picked up (or degraded around) without
/// rebuilding the backend.
pub(crate) struct LdapDirectoryBackend {
    directory: LdapDirectory,
    keyring: Arc<dyn KeyringStore>,
    search: Arc<dyn DirectorySearch>,
}

impl LdapDirectoryBackend {
    /// Builds the provider for one configured directory. Constructing it
    /// touches no network and no keyring.
    pub(crate) fn new(directory: &LdapDirectory, keyring: Arc<dyn KeyringStore>) -> Self {
        LdapDirectoryBackend {
            directory: directory.clone(),
            keyring,
            search: Arc::new(LdapConnSearch),
        }
    }

    /// A provider over an explicit searcher, for tests driving a fake.
    #[cfg(test)]
    fn with_searcher(directory: &LdapDirectory, keyring: Arc<dyn KeyringStore>, search: Arc<dyn DirectorySearch>) -> Self {
        LdapDirectoryBackend {
            directory: directory.clone(),
            keyring,
            search,
        }
    }

    /// Whether the directory is searched. The 4.7.6 wiring assembles the
    /// running backends with this check, so a disabled or hand-written entry
    /// contributes nothing at no network cost.
    pub(crate) fn enabled(&self) -> bool {
        self.directory.enabled
    }

    /// The bind password for this directory, or an empty string when the
    /// config asks for an anonymous bind. A non-empty `bind_dn` whose secret
    /// is missing from the keyring is an error the search degrades on.
    async fn bind_password(&self) -> Result<String, String> {
        if self.directory.bind_dn.trim().is_empty() {
            return Ok(String::new());
        }
        self.keyring.load_password(&AccountId(self.directory.id.clone()), PROTOCOL_LDAP).await
    }
}

#[async_trait]
impl DirectoryBackend for LdapDirectoryBackend {
    /// Never errors: a missing keyring secret, a dead host, a refused bind or
    /// a timeout all come back as an empty list, and the recipient popover
    /// simply shows stored contacts only.
    async fn search(&self, query: &str, limit: usize) -> Vec<DirectoryPerson> {
        let password = match self.bind_password().await {
            Ok(password) => password,
            Err(error) => {
                tracing::debug!(directory = %self.directory.id, "LDAP bind password unavailable: {error}");
                return Vec::new();
            }
        };
        let mail_attr = mail_attr(&self.directory);
        let name_attr = name_attr(&self.directory);
        let (url, start_tls) = connection_plan(&self.directory.host, self.directory.port, self.directory.tls);
        let plan = LdapQuery {
            url,
            start_tls,
            bind_dn: self.directory.bind_dn.clone(),
            password,
            base_dn: self.directory.base_dn.clone(),
            filter: build_filter(query, &self.directory.filter, mail_attr, name_attr),
            attrs: search_attrs(mail_attr, name_attr),
            limit,
        };
        self.search
            .search(&plan)
            .await
            .iter()
            .filter_map(|entry| entry_to_person(entry, mail_attr, name_attr))
            .take(limit)
            .collect()
    }
}

/// One search's resolved parameters: everything the network step needs after
/// the config, keyring and pure helpers have done their part.
#[derive(Clone, Debug)]
struct LdapQuery {
    url: String,
    start_tls: bool,
    bind_dn: String,
    password: String,
    base_dn: String,
    filter: String,
    attrs: Vec<String>,
    limit: usize,
}

/// One raw entry as the searcher hands it over: attribute name → values.
/// Names are normalised to lower case at construction, so lookups behave the
/// way LDAP attribute names do (case-insensitively).
#[derive(Clone, Debug, PartialEq, Eq)]
struct LdapEntry {
    attrs: HashMap<String, Vec<String>>,
}

impl LdapEntry {
    fn new(attrs: HashMap<String, Vec<String>>) -> Self {
        LdapEntry {
            attrs: attrs.into_iter().map(|(name, values)| (name.to_lowercase(), values)).collect(),
        }
    }

    /// The first value of `attr`, or `None`.
    fn value(&self, attr: &str) -> Option<&str> {
        self.attrs.get(&attr.to_lowercase()).and_then(|values| values.first()).map(String::as_str)
    }
}

/// The network step of an LDAP lookup, behind a trait so the backend is
/// unit-tested against a fake and never needs a live server. Implementations
/// return raw entries and treat every failure as "no entries": the caller
/// only ever degrades.
#[async_trait]
trait DirectorySearch: Send + Sync {
    async fn search(&self, query: &LdapQuery) -> Vec<LdapEntry>;
}

/// The production searcher: ldap3's async connection. A fresh connection per
/// search keeps the backend stateless - no stale socket or expired bind to
/// re-auth - and directory lookups are debounced and prefix-cached upstream,
/// so the extra round trip is paid rarely.
struct LdapConnSearch;

#[async_trait]
impl DirectorySearch for LdapConnSearch {
    async fn search(&self, query: &LdapQuery) -> Vec<LdapEntry> {
        match run_ldap_search(query).await {
            Ok(entries) => entries,
            Err(error) => {
                tracing::debug!(url = %query.url, "LDAP search failed: {error}");
                Vec::new()
            }
        }
    }
}

/// Runs one connect → (StartTLS) → bind → search → unbind against the server,
/// with the connect and every operation bounded by [`LDAP_TIMEOUT`].
async fn run_ldap_search(query: &LdapQuery) -> Result<Vec<LdapEntry>, ldap3::LdapError> {
    let settings = LdapConnSettings::new().set_conn_timeout(LDAP_TIMEOUT).set_starttls(query.start_tls);
    let (conn, mut ldap) = LdapConnAsync::with_settings(settings, &query.url).await?;
    ldap3::drive!(conn);
    if !query.bind_dn.trim().is_empty() {
        ldap.with_timeout(LDAP_TIMEOUT).simple_bind(&query.bind_dn, &query.password).await?.success()?;
    }
    let attrs: Vec<&str> = query.attrs.iter().map(String::as_str).collect();
    let options = SearchOptions::new().sizelimit(clamp_limit(query.limit));
    let (entries, _result) = ldap
        .with_timeout(LDAP_TIMEOUT)
        .with_search_options(options)
        .search(&query.base_dn, Scope::Subtree, &query.filter, attrs)
        .await?
        .success()?;
    let _ = ldap.unbind().await;
    Ok(entries.into_iter().map(|entry| LdapEntry::new(SearchEntry::construct(entry).attrs)).collect())
}

/// The server-side entry cap as `ldap3` wants it: at least one (zero means
/// "no limit" to LDAP) and never past an `i32`.
fn clamp_limit(limit: usize) -> i32 {
    limit.clamp(1, i32::MAX as usize) as i32
}

/// The connect URL and whether to negotiate StartTLS for a directory: an
/// untrusted (`tls = false`) entry connects in the clear; an LDAPS port (636,
/// or AD's global-catalog 3269) gets implicit TLS; any other port with `tls`
/// uses StartTLS over a plain connection.
fn connection_plan(host: &str, port: u16, tls: bool) -> (String, bool) {
    if !tls {
        (format!("ldap://{host}:{port}"), false)
    } else if LDAPS_PORTS.contains(&port) {
        (format!("ldaps://{host}:{port}"), false)
    } else {
        (format!("ldap://{host}:{port}"), true)
    }
}

/// The configured address attribute, or the built-in default when empty.
fn mail_attr(directory: &LdapDirectory) -> &str {
    let attr = directory.mail_attr.trim();
    if attr.is_empty() {
        DEFAULT_MAIL_ATTR
    } else {
        attr
    }
}

/// The configured display-name attribute, or the built-in default when empty.
fn name_attr(directory: &LdapDirectory) -> &str {
    let attr = directory.name_attr.trim();
    if attr.is_empty() {
        DEFAULT_NAME_ATTR
    } else {
        attr
    }
}

/// The attributes any entry needs to be mapped: the configured address and
/// name attributes, the `cn` display fallback, and the title/company fields
/// [`DirectoryPerson`] carries. De-duplicated case-insensitively, since LDAP
/// attribute names are.
fn search_attrs(mail_attr: &str, name_attr: &str) -> Vec<String> {
    let mut attrs = vec![mail_attr.to_string(), name_attr.to_string()];
    for attr in ["cn", "title", "company", "o"] {
        if !attrs.iter().any(|existing| existing.eq_ignore_ascii_case(attr)) {
            attrs.push(attr.to_string());
        }
    }
    attrs
}

/// Escapes `input` per RFC 4515, so a user's typed prefix can't change the
/// filter's structure: the characters that would (`\ * ( )` and NUL) become
/// their `\XX` hex forms.
fn filter_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '\\' => out.push_str("\\5c"),
            '*' => out.push_str("\\2a"),
            '(' => out.push_str("\\28"),
            ')' => out.push_str("\\29"),
            '\0' => out.push_str("\\00"),
            _ => out.push(c),
        }
    }
    out
}

/// Wraps the config's extra filter in parentheses when the user left them
/// off, so [`build_filter`]'s AND composition stays well-formed; an empty
/// filter (the default) is `None`.
fn normalise_filter(filter: &str) -> Option<String> {
    let filter = filter.trim();
    if filter.is_empty() {
        return None;
    }
    if filter.starts_with('(') && filter.ends_with(')') {
        Some(filter.to_string())
    } else {
        Some(format!("({filter})"))
    }
}

/// The LDAP filter for one prefix: the configured extra filter (if any) ANDed
/// with a match of the RFC 4515-escaped prefix against the address and name
/// attributes. Pure, so the escaping and composition are unit-tested.
fn build_filter(query: &str, extra: &str, mail_attr: &str, name_attr: &str) -> String {
    let query = filter_escape(query);
    let match_filter = format!("(|({mail_attr}=*{query}*)({name_attr}=*{query}*))");
    match normalise_filter(extra) {
        Some(extra) => format!("(&{extra}{match_filter})"),
        None => match_filter,
    }
}

/// Maps one raw entry to a [`DirectoryPerson`], or `None` when it has no
/// usable address (a group or device entry the filter let through).
/// `name_attr` falls back to `cn`, so directories whose entries lack the
/// configured display attribute still show a name.
fn entry_to_person(entry: &LdapEntry, mail_attr: &str, name_attr: &str) -> Option<DirectoryPerson> {
    let address = entry.value(mail_attr)?.trim().to_string();
    if address.is_empty() {
        return None;
    }
    let name = non_empty(entry.value(name_attr)).or_else(|| non_empty(entry.value("cn")));
    let title = non_empty(entry.value("title"));
    let company = non_empty(entry.value("company")).or_else(|| non_empty(entry.value("o")));
    Some(DirectoryPerson {
        id: None,
        name,
        address,
        title,
        company,
    })
}

/// A trimmed, non-empty attribute value, or `None`.
fn non_empty(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn directory() -> LdapDirectory {
        LdapDirectory {
            id: "ldap:test".to_string(),
            account_id: AccountId("account_1".to_string()),
            display_name: "Company LDAP".to_string(),
            host: "ldap.example.com".to_string(),
            port: 389,
            tls: false,
            bind_dn: "cn=bind,dc=example,dc=com".to_string(),
            base_dn: "dc=example,dc=com".to_string(),
            filter: String::new(),
            mail_attr: String::new(),
            name_attr: String::new(),
            enabled: true,
        }
    }

    fn entry(attrs: &[(&str, &[&str])]) -> LdapEntry {
        LdapEntry::new(
            attrs
                .iter()
                .map(|(name, values)| (name.to_string(), values.iter().map(|value| value.to_string()).collect()))
                .collect(),
        )
    }

    struct FakeKeyring(Result<String, String>);

    #[async_trait]
    impl KeyringStore for FakeKeyring {
        async fn load_password(&self, _account_id: &AccountId, _protocol: &str) -> Result<String, String> {
            self.0.clone()
        }

        async fn store_password(&self, _account_id: &AccountId, _protocol: &str, _password: &str) -> Result<(), String> {
            Ok(())
        }

        async fn delete_account(&self, _account_id: &AccountId) -> Result<(), String> {
            Ok(())
        }
    }

    struct FakeSearch {
        entries: Vec<LdapEntry>,
        queries: Arc<Mutex<Vec<LdapQuery>>>,
    }

    impl FakeSearch {
        fn new(entries: Vec<LdapEntry>) -> Self {
            FakeSearch {
                entries,
                queries: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl DirectorySearch for FakeSearch {
        async fn search(&self, query: &LdapQuery) -> Vec<LdapEntry> {
            self.queries.lock().unwrap().push(query.clone());
            self.entries.clone()
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
    }

    #[test]
    fn filter_escape_neutralises_rfc_4515_specials() {
        assert_eq!(filter_escape("a*b(c)d\\e"), "a\\2ab\\28c\\29d\\5ce");
        assert_eq!(filter_escape("plain"), "plain");
        assert_eq!(filter_escape("nul\0byte"), "nul\\00byte");
    }

    #[test]
    fn build_filter_uses_the_configured_attrs_and_ands_the_extra_filter() {
        assert_eq!(build_filter("gra", "", "mail", "displayName"), "(|(mail=*gra*)(displayName=*gra*))");
        assert_eq!(
            build_filter("gra", "objectClass=person", "mail", "displayName"),
            "(&(objectClass=person)(|(mail=*gra*)(displayName=*gra*)))"
        );
        // An already-parenthesised extra filter passes through as written,
        // and the query's own metacharacters stay escaped.
        assert_eq!(
            build_filter("x)", "(objectClass=person)", "uid", "cn"),
            "(&(objectClass=person)(|(uid=*x\\29*)(cn=*x\\29*)))"
        );
    }

    #[test]
    fn connection_plan_picks_implicit_tls_starttls_or_plain() {
        assert_eq!(connection_plan("ldap.example.com", 389, false), ("ldap://ldap.example.com:389".to_string(), false));
        assert_eq!(connection_plan("ldap.example.com", 636, true), ("ldaps://ldap.example.com:636".to_string(), false));
        assert_eq!(connection_plan("ldap.example.com", 3269, true), ("ldaps://ldap.example.com:3269".to_string(), false));
        assert_eq!(connection_plan("ldap.example.com", 389, true), ("ldap://ldap.example.com:389".to_string(), true));
        // The flag, not the port, decides: tls=false connects in the clear.
        assert_eq!(connection_plan("ldap.example.com", 636, false), ("ldap://ldap.example.com:636".to_string(), false));
    }

    #[test]
    fn entry_to_person_maps_fields_and_falls_back_to_cn() {
        let grace = entry(&[("MAIL", &["Grace@Example.com"]), ("cn", &["Grace Hopper"]), ("title", &["Admiral"]), ("o", &["US Navy"])]);
        let person = entry_to_person(&grace, "mail", "displayName").unwrap();
        assert_eq!(person.address, "Grace@Example.com");
        assert_eq!(person.name.as_deref(), Some("Grace Hopper"));
        assert_eq!(person.title.as_deref(), Some("Admiral"));
        assert_eq!(person.company.as_deref(), Some("US Navy"));

        // The configured display attribute wins over cn when present.
        let with_display = entry(&[("mail", &["ada@example.com"]), ("displayName", &["Ada Lovelace"]), ("cn", &["ignored"])]);
        let person = entry_to_person(&with_display, "mail", "displayName").unwrap();
        assert_eq!(person.name.as_deref(), Some("Ada Lovelace"));

        // company is preferred over its OpenLDAP alias o.
        let with_both = entry(&[("mail", &["a@example.com"]), ("company", &["Acme"]), ("o", &["Ignored"])]);
        assert_eq!(entry_to_person(&with_both, "mail", "displayName").unwrap().company.as_deref(), Some("Acme"));
    }

    #[test]
    fn entry_to_person_skips_entries_without_an_address() {
        assert!(entry_to_person(&entry(&[("cn", &["Group"])]), "mail", "displayName").is_none());
        assert!(entry_to_person(&entry(&[("mail", &["   "])]), "mail", "displayName").is_none());
        assert!(entry_to_person(&entry(&[("mail", &[""])]), "mail", "displayName").is_none());
    }

    #[test]
    fn search_runs_the_pure_plan_and_maps_the_hits() {
        block_on(async {
            let searcher = Arc::new(FakeSearch::new(vec![
                entry(&[("mail", &["grace@example.com"]), ("displayName", &["Grace Hopper"])]),
                entry(&[("mail", &["no-name@example.com"])]),
            ]));
            let backend = LdapDirectoryBackend::with_searcher(&directory(), Arc::new(FakeKeyring(Ok("bind-secret".to_string()))), searcher.clone());
            let hits = backend.search("grace", 8).await;
            assert_eq!(hits.len(), 2);
            assert_eq!(hits[0].address, "grace@example.com");
            assert_eq!(hits[0].name.as_deref(), Some("Grace Hopper"));
            assert!(hits[1].name.is_none());

            let queries = searcher.queries.lock().unwrap();
            assert_eq!(queries.len(), 1);
            let query = &queries[0];
            assert_eq!(query.url, "ldap://ldap.example.com:389");
            assert!(!query.start_tls);
            assert_eq!(query.bind_dn, "cn=bind,dc=example,dc=com");
            assert_eq!(query.password, "bind-secret");
            assert_eq!(query.base_dn, "dc=example,dc=com");
            assert_eq!(query.filter, "(|(mail=*grace*)(displayName=*grace*))");
            assert_eq!(query.limit, 8);
            // The configured attributes ride along, plus the mapping fallbacks.
            assert!(query.attrs.iter().any(|attr| attr == "cn"));
            assert!(query.attrs.iter().any(|attr| attr == "title"));
        });
    }

    #[test]
    fn search_caps_hits_at_the_limit_and_skips_addressless_entries() {
        block_on(async {
            let searcher = Arc::new(FakeSearch::new(vec![
                entry(&[("mail", &["a@example.com"])]),
                entry(&[("cn", &["No Address"])]),
                entry(&[("mail", &["b@example.com"])]),
            ]));
            let backend = LdapDirectoryBackend::with_searcher(&directory(), Arc::new(FakeKeyring(Ok("bind-secret".to_string()))), searcher.clone());
            let hits = backend.search("someone", 1).await;
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].address, "a@example.com");
            assert_eq!(searcher.queries.lock().unwrap()[0].limit, 1);
        });
    }

    #[test]
    fn search_degrades_to_no_hits_when_the_bind_password_is_missing() {
        block_on(async {
            let searcher = Arc::new(FakeSearch::new(vec![entry(&[("mail", &["a@example.com"])])]));
            let backend = LdapDirectoryBackend::with_searcher(&directory(), Arc::new(FakeKeyring(Err("no secret".to_string()))), searcher.clone());
            assert!(backend.search("grace", 8).await.is_empty());
            // The search was abandoned before any network work.
            assert!(searcher.queries.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn an_anonymous_directory_needs_no_stored_password() {
        block_on(async {
            let mut anonymous = directory();
            anonymous.bind_dn = String::new();
            let searcher = Arc::new(FakeSearch::new(vec![entry(&[("mail", &["a@example.com"])])]));
            let backend = LdapDirectoryBackend::with_searcher(&anonymous, Arc::new(FakeKeyring(Err("no secret".to_string()))), searcher.clone());
            assert_eq!(backend.search("grace", 8).await.len(), 1);
            assert_eq!(searcher.queries.lock().unwrap()[0].password, "");
        });
    }

    #[test]
    fn enabled_reflects_the_stored_configuration() {
        let keyring = Arc::new(FakeKeyring(Ok(String::new())));
        let searcher = Arc::new(FakeSearch::new(Vec::new()));
        assert!(LdapDirectoryBackend::with_searcher(&directory(), keyring.clone(), searcher.clone()).enabled());
        let mut disabled = directory();
        disabled.enabled = false;
        assert!(!LdapDirectoryBackend::with_searcher(&disabled, keyring, searcher).enabled());
    }
}
