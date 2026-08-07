use chrono::{DateTime, Utc};
use lookout_core::{AccountId, CalendarEvent, CalendarId, CalendarInfo, CalendarTask, VCard};
use reqwest::Method;

use crate::config::Credential;
use crate::error::{Error, Result};
use crate::ical;
use crate::xml::{self, DavResponse, NS_CARDDAV};

const NS_DAV: &[u8] = b"DAV:";
const NS_CALDAV: &[u8] = b"urn:ietf:params:xml:ns:caldav";
const NS_APPLE_ICAL: &[u8] = b"http://apple.com/ns/ical/";

const ADDRESSBOOK_PROPS: [&str; 2] = ["D:displayname", "D:resourcetype"];

/// The maximum size of a downloaded webcal feed body. Feeds are untrusted
/// URLs (a misconfigured subscription can point at anything), so a body over
/// this is rejected rather than buffered.
pub const MAX_FEED_BYTES: usize = 5 * 1024 * 1024;

/// The maximum number of characters of a server response body embedded in a
/// user-facing error message. Servers explain their 4xxes in the body, but
/// the whole thing (often a whole HTML error page) belongs in the log, not a
/// toast.
const MAX_ERROR_SNIPPET_CHARS: usize = 300;

/// Reduces a server response body to a display-safe snippet for an error
/// message. Server error pages are HTML (`<title>`s, tags, `\r\n` line
/// endings) and GTK widgets reject control characters outright - a raw body
/// in a toast or status label both renders as garbage and can trip a GTK
/// "Failed to set text" warning. Tags are stripped (the title text survives),
/// control characters other than `\n`/`\t` are dropped, whitespace runs are
/// collapsed, and the result is capped at [`MAX_ERROR_SNIPPET_CHARS`].
fn sanitize_snippet(body: &str) -> String {
    let mut out = String::with_capacity(body.len().min(MAX_ERROR_SNIPPET_CHARS + 32));
    let mut in_tag = false;
    let mut last_was_space = false;
    for c in body.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            c if c.is_control() && !matches!(c, '\n' | '\t') => {}
            c if c.is_whitespace() => {
                if !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
            }
            c => {
                out.push(c);
                last_was_space = false;
            }
        }
        if out.len() >= MAX_ERROR_SNIPPET_CHARS {
            break;
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// Normalizes a user-supplied calendar feed URL for fetching: the
/// `webcal://`/`webcals://` schemes (RFC 5545's `calconnect` aliases for
/// `http`/`https`, the only scheme an actual webcal feed can be served on)
/// are rewritten to their real equivalents; anything else is passed through
/// and must already be `http`/`https`.
pub fn normalize_webcal_url(raw: &str) -> Result<reqwest::Url> {
    let raw = raw.trim();
    let rewritten = match raw.find("://") {
        Some(end) => match raw[..end].to_ascii_lowercase().as_str() {
            "webcal" => format!("http{}", &raw[end..]),
            "webcals" => format!("https{}", &raw[end..]),
            _ => raw.to_string(),
        },
        None => raw.to_string(),
    };
    let url = reqwest::Url::parse(&rewritten).map_err(|e| Error::Discovery(format!("invalid calendar feed URL {raw:?}: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Discovery(format!(
            "unsupported scheme {:?} - calendar feeds must be http(s) (webcal:// is accepted)",
            url.scheme()
        )));
    }
    Ok(url)
}

/// Fetches a public `.ics` document (a webcal feed, or an import source) over
/// plain HTTP/HTTPS with no credentials - webcal feeds are public by nature,
/// and auth-protected feeds are deliberately out of scope. `url` should come
/// from [`normalize_webcal_url`] (the scheme rewrite is not re-applied here).
/// Rejects responses advertising a body larger than [`MAX_FEED_BYTES`] before
/// buffering, and post-checks the actual size for servers that lie about
/// `Content-Length`.
pub async fn fetch_webcal_ics(http: &reqwest::Client, url: &reqwest::Url) -> Result<String> {
    let request_url = url.clone();
    let response = http.get(url.clone()).send().await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await?;
        let snippet = sanitize_snippet(&text);
        return Err(Error::Discovery(format!("HTTP {status} for {request_url}: {snippet}")));
    }
    if response.content_length().is_some_and(|len| len > MAX_FEED_BYTES as u64) {
        return Err(Error::Discovery(format!("feed {request_url} exceeds the {MAX_FEED_BYTES}-byte size limit")));
    }
    let body = response.bytes().await?;
    if body.len() > MAX_FEED_BYTES {
        return Err(Error::Discovery(format!("feed {request_url} exceeds the {MAX_FEED_BYTES}-byte size limit")));
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

#[derive(Debug, Clone)]
pub struct AddressBookInfo {
    pub account_id: AccountId,
    pub display_name: String,
    pub href: String,
}

impl AddressBookInfo {
    fn new(account_id: &AccountId, href: &str, display_name: String) -> Self {
        Self {
            account_id: account_id.clone(),
            display_name,
            href: href.to_string(),
        }
    }
}

/// A contact as stored in an address book: the parsed [`VCard`] plus the
/// collection-relative `href` and current `getetag` of its server-side
/// object. The write path needs both - a card fetched without them can't be
/// PUT/DELETEd back with the `If-Match` precondition guard.
#[derive(Debug, Clone)]
pub struct ContactRecord {
    pub href: String,
    pub etag: Option<String>,
    pub card: VCard,
}

impl DavClient {
    pub async fn discover_addressbook_home(&self, credential: &Credential) -> Result<String> {
        let principal = self
            .propfind(self.base_url.clone(), 0, credential, &["D:current-user-principal"])
            .await?
            .into_iter()
            .find_map(|r| r.prop(NS_DAV, "current-user-principal").map(str::to_string));

        let principal_href = match principal {
            Some(href) => href,
            None => {
                tracing::warn!("server didn't answer current-user-principal; treating the configured URL as the addressbook-home-set directly");
                return Ok(self.base_url.to_string());
            }
        };
        let principal_url = self.resolve(&principal_href)?;

        let home_set = self
            .propfind(principal_url, 0, credential, &["CD:addressbook-home-set"])
            .await?
            .into_iter()
            .find_map(|r| r.prop(NS_CARDDAV, "addressbook-home-set").map(str::to_string));

        match home_set {
            Some(href) => Ok(href),
            None => {
                tracing::warn!("server didn't answer addressbook-home-set; treating the configured URL as the addressbook-home-set directly");
                Ok(self.base_url.to_string())
            }
        }
    }

    pub async fn list_addressbooks(&self, home_set_href: &str, account_id: &AccountId, credential: &Credential) -> Result<Vec<AddressBookInfo>> {
        let home_url = self.resolve(home_set_href)?;
        let responses = self.propfind(home_url, 1, credential, &ADDRESSBOOK_PROPS).await?;

        Ok(responses
            .into_iter()
            .filter(|r| r.prop(NS_DAV, "resourcetype").unwrap_or("").contains("addressbook"))
            .map(|r| AddressBookInfo::new(account_id, &r.href, r.prop(NS_DAV, "displayname").unwrap_or("").to_string()))
            .collect())
    }

    pub async fn sync_addressbook(&self, url: reqwest::Url, credential: &Credential, sync_token: Option<&str>) -> Result<Vec<DavResponse>> {
        let body = xml::build_sync_collection_body(sync_token, &["D:getetag", "CD:address-data"]);
        self.sync_collection(url, 1, credential, body).await
    }

    /// Runs an RFC 6578 `sync-collection` REPORT for one address book,
    /// returning `(changed records, deleted hrefs, next sync token)`. The
    /// caller stores the token (and passes it back on the next poll) so only
    /// the members that actually changed since the last sync are re-fetched;
    /// a `None` token is a cold start. Removed members are reported as
    /// response-level `404` statuses with no properties, which is exactly why
    /// [`xml::DavResponse::status`] exists. A stored token that the server no
    /// longer recognises surfaces as a 4xx HTTP error from
    /// [`Self::sync_collection`] - callers fall back to a full
    /// [`Self::fetch_addressbook_contacts_with_meta`] refetch, which resets
    /// the token.
    pub async fn sync_addressbook_delta(
        &self,
        addressbook: &AddressBookInfo,
        credential: &Credential,
        sync_token: Option<&str>,
    ) -> Result<(Vec<ContactRecord>, Vec<String>, Option<String>)> {
        let url = self.resolve(&addressbook.href)?;
        let body = xml::build_sync_collection_body(sync_token, &["D:getetag", "CD:address-data"]);
        let (responses, next_token) = self.send_xml_request_with_token("REPORT", url, 1, credential, body).await?;
        let mut changed = Vec::new();
        let mut deleted = Vec::new();
        for response in responses {
            if response.status.as_deref().is_some_and(|s| s.contains("404")) {
                deleted.push(response.href);
                continue;
            }
            let Some(data) = response.prop(NS_CARDDAV, "address-data") else { continue };
            let href = response.href.clone();
            let etag = response.prop(NS_DAV, "getetag").map(str::to_string);
            match VCard::parse(data) {
                Ok(card) => changed.push(ContactRecord { href, etag, card }),
                Err(e) => tracing::warn!("skipping unparseable vCard at {href:?}: {e}"),
            }
        }
        Ok((changed, deleted, next_token))
    }

    /// Fetches every vCard in an address book: a `PROPFIND` (Depth: 1) to
    /// enumerate the collection's member hrefs, then an
    /// `addressbook-multiget` REPORT for their vCard bodies. Deliberately not
    /// `sync_addressbook`'s `sync-collection` (this app never stores a sync
    /// token between polls, and Google's CardDAV rejects a cold-start
    /// `sync-collection` outright) nor an `addressbook-query` with an empty
    /// "match everything" filter (confirmed against Google's CardDAV: it
    /// returns a bare, response-less `<multistatus/>` for that on an account
    /// known to have contacts, i.e. it reads the empty filter as "match
    /// nothing" despite RFC 6352's own example using exactly that). Multiget
    /// avoids filter semantics entirely by asking for specific known hrefs.
    pub async fn fetch_addressbook_contacts(&self, addressbook: &AddressBookInfo, credential: &Credential) -> Result<Vec<DavResponse>> {
        let url = self.resolve(&addressbook.href)?;
        let members = self.propfind(url.clone(), 1, credential, &["D:getetag"]).await?;
        let own_href = addressbook.href.trim_end_matches('/');
        let hrefs: Vec<String> = members.into_iter().map(|r| r.href).filter(|href| href.trim_end_matches('/') != own_href).collect();
        if hrefs.is_empty() {
            return Ok(Vec::new());
        }
        let body = xml::build_addressbook_multiget_body(&hrefs, &["D:getetag", "CD:address-data"]);
        self.report(url, 1, credential, body).await
    }

    pub async fn fetch_addressbook_vcards(&self, addressbook: &AddressBookInfo, credential: &Credential) -> Result<Vec<VCard>> {
        Ok(self
            .fetch_addressbook_contacts_with_meta(addressbook, credential)
            .await?
            .into_iter()
            .map(|record| record.card)
            .collect())
    }

    /// [`Self::fetch_addressbook_vcards`] with the server-side object metadata
    /// kept: each card's `href` and current `getetag`, so the caller can PUT
    /// or DELETE it back with the `If-Match` precondition guard. One
    /// malformed/unsupported card is skipped (logged) like the plain-fetch
    /// path, never fatal.
    pub async fn fetch_addressbook_contacts_with_meta(&self, addressbook: &AddressBookInfo, credential: &Credential) -> Result<Vec<ContactRecord>> {
        let responses = self.fetch_addressbook_contacts(addressbook, credential).await?;
        let mut records = Vec::new();
        for response in responses {
            let Some(data) = response.prop(NS_CARDDAV, "address-data") else { continue };
            let href = response.href.clone();
            let etag = response.prop(NS_DAV, "getetag").map(str::to_string);
            match VCard::parse(data) {
                Ok(card) => records.push(ContactRecord { href, etag, card }),
                Err(e) => tracing::warn!("skipping unparseable vCard at {href:?}: {e}"),
            }
        }
        Ok(records)
    }
}

/// A thin CalDAV/WebDAV HTTP client for one account's calendar endpoint.
/// Holds no credentials - a fresh [`Credential`] is passed to every request,
/// matching `lookout-mail`'s "never cache credentials ourselves" convention.
pub struct DavClient {
    http: reqwest::Client,
    base_url: reqwest::Url,
    /// Paired with `Credential::Password` for HTTP Basic Auth; unused for
    /// `Credential::OAuth2AccessToken` (Bearer auth needs no username).
    username: String,
}

impl DavClient {
    pub fn new(base_url: &str, accept_ssl_errors: bool, username: String) -> Result<Self> {
        let mut base_url = reqwest::Url::parse(base_url).map_err(|e| Error::Discovery(format!("invalid base URL {base_url:?}: {e}")))?;
        // The username travels in the Authorization header, never in the URL.
        // GOA has been observed to embed it as userinfo (`https://user@host/`),
        // which some front-ends reject; scrub it so every request goes out clean.
        let _ = base_url.set_username("");
        let http = reqwest::Client::builder().danger_accept_invalid_certs(accept_ssl_errors).build()?;
        Ok(Self { http, base_url, username })
    }

    /// RFC 4791 discovery: PROPFIND `base_url` for `current-user-principal`,
    /// then PROPFIND that principal URL for `calendar-home-set`. Falls back
    /// to treating `base_url` itself as the home-set if a server skips
    /// `current-user-principal` (logged, not a hard error - some minimal
    /// CalDAV servers do this).
    pub async fn discover_calendar_home(&self, credential: &Credential) -> Result<String> {
        let principal = self
            .propfind(self.base_url.clone(), 0, credential, &["D:current-user-principal"])
            .await?
            .into_iter()
            .find_map(|r| r.prop(NS_DAV, "current-user-principal").map(str::to_string));

        let Some(principal_href) = principal else {
            tracing::warn!("server didn't answer current-user-principal; treating the configured URL as the calendar-home-set directly");
            return Ok(self.base_url.to_string());
        };
        let principal_url = self.resolve(&principal_href)?;

        let home_set = self
            .propfind(principal_url, 0, credential, &["C:calendar-home-set"])
            .await?
            .into_iter()
            .find_map(|r| r.prop(NS_CALDAV, "calendar-home-set").map(str::to_string));

        match home_set {
            Some(href) => Ok(href),
            None => {
                tracing::warn!("server didn't answer calendar-home-set; treating the configured URL as the calendar-home-set directly");
                Ok(self.base_url.to_string())
            }
        }
    }

    /// PROPFIND (Depth: 1) the calendar-home-set collection, keeping only
    /// child resources whose `resourcetype` includes a `calendar` marker.
    /// `CalendarInfo::supports_tasks` comes from the collection's
    /// `supported-calendar-component-set` (RFC 4791 §5.2.3) - a server that
    /// doesn't advertise `VTODO` (Google's CalDAV is `VEVENT`-only) can't
    /// store tasks, so the task picker and sync skip it rather than PUTing
    /// into a 403.
    pub async fn list_calendars(&self, home_set_href: &str, account_id: &AccountId, credential: &Credential) -> Result<Vec<CalendarInfo>> {
        let home_url = self.resolve(home_set_href)?;
        let responses = self
            .propfind(
                home_url,
                1,
                credential,
                &["D:displayname", "D:resourcetype", "IC:calendar-color", "C:supported-calendar-component-set"],
            )
            .await?;

        Ok(responses
            .into_iter()
            .filter(|r| r.prop(NS_DAV, "resourcetype").unwrap_or("").contains("calendar"))
            .map(|r| {
                // Absent property (a non-compliant server) means "don't know" -
                // assume tasks are accepted rather than hiding a calendar that
                // could hold them; an explicit component list without VTODO
                // (Google) is a definitive no.
                let advertised = r.prop(NS_CALDAV, "supported-calendar-component-set");
                let supports_tasks = advertised.is_none() || r.supports_component(NS_CALDAV, "supported-calendar-component-set", "VTODO");
                CalendarInfo {
                    id: CalendarId::new(account_id, &r.href),
                    account_id: account_id.clone(),
                    display_name: r.prop(NS_DAV, "displayname").unwrap_or("").to_string(),
                    color: r.prop(NS_APPLE_ICAL, "calendar-color").map(str::to_string),
                    href: r.href.clone(),
                    supports_tasks,
                }
            })
            .collect())
    }

    /// `calendar-query` REPORT scoped to one calendar collection, filtered to
    /// `VEVENT`s overlapping `[start, end)`.
    pub async fn fetch_events_in_range(&self, calendar: &CalendarInfo, start: DateTime<Utc>, end: DateTime<Utc>, credential: &Credential) -> Result<Vec<CalendarEvent>> {
        let cal_url = self.resolve(&calendar.href)?;
        let body = xml::build_calendar_query_body(start, end);
        let responses = self.report(cal_url, 1, credential, body).await?;

        Ok(responses
            .into_iter()
            .filter_map(|r| {
                let href = r.href.clone();
                let etag = r.prop(NS_DAV, "getetag").map(str::to_string);
                let data = r.prop(NS_CALDAV, "calendar-data")?.to_string();
                Some((href, etag, data))
            })
            .flat_map(|(href, etag, ics)| ical::parse_vevents_with_meta(&calendar.id, &ics, Some(&href), etag.as_deref()))
            .collect())
    }

    /// `todo-query` REPORT scoped to one calendar collection, requesting
    /// every `VTODO` in it - the `fetch_events_in_range` counterpart for
    /// tasks (see [`xml::build_todo_query_body`] for why there's no
    /// time-range filter).
    pub async fn fetch_tasks(&self, calendar: &CalendarInfo, credential: &Credential) -> Result<Vec<CalendarTask>> {
        let cal_url = self.resolve(&calendar.href)?;
        let body = xml::build_todo_query_body();
        let responses = self.report(cal_url, 1, credential, body).await?;

        Ok(responses
            .into_iter()
            .filter_map(|r| {
                let href = r.href.clone();
                let etag = r.prop(NS_DAV, "getetag").map(str::to_string);
                let data = r.prop(NS_CALDAV, "calendar-data")?.to_string();
                Some((href, etag, data))
            })
            .flat_map(|(href, etag, ics)| ical::parse_vtodos_with_meta(&calendar.id, &ics, Some(&href), etag.as_deref()))
            .collect())
    }

    /// Stores `ics` as a calendar object at `href` via `PUT` (RFC 4791 §5.3).
    /// `etag` is the resource's current `getetag` for an update - sent as
    /// `If-Match`, so a write based on a stale copy fails with HTTP 412 rather
    /// than silently clobbering a concurrent change. `None` marks a create and
    /// sends `If-None-Match: *`, refusing to overwrite an existing resource.
    /// Returns the server's new etag (the `ETag` response header), if any.
    pub async fn put_calendar_object(&self, href: &str, ics: &str, credential: &Credential, etag: Option<&str>) -> Result<Option<String>> {
        self.put_object(href, ics, "text/calendar; charset=utf-8", credential, etag).await
    }

    /// Deletes the calendar object at `href` via `DELETE` (RFC 4791 §5.3.4).
    /// An optional `etag` is sent as `If-Match` so deleting a resource that
    /// has changed since it was fetched fails loudly instead of silently
    /// removing a concurrent edit.
    pub async fn delete_calendar_object(&self, href: &str, credential: &Credential, etag: Option<&str>) -> Result<()> {
        self.delete_object(href, credential, etag).await
    }

    /// Stores `card` as a vCard object at `href` via `PUT` (RFC 6352 §8.2).
    /// The same `If-Match`/`If-None-Match` precondition semantics as
    /// [`Self::put_calendar_object`]: `etag` guards an update against
    /// clobbering a concurrent change (HTTP 412), `None` creates the object
    /// without overwriting an existing one. Returns the server's new etag.
    pub async fn put_contact_vcard(&self, href: &str, card: &VCard, credential: &Credential, etag: Option<&str>) -> Result<Option<String>> {
        self.put_object(href, &card.serialize(), "text/vcard; charset=utf-8", credential, etag).await
    }

    /// Deletes the vCard object at `href` via `DELETE` (RFC 6352 §8.3). An
    /// optional `etag` is sent as `If-Match` so deleting a card that has
    /// changed since it was fetched fails loudly instead of silently removing
    /// a concurrent edit.
    pub async fn delete_contact_vcard(&self, href: &str, credential: &Credential, etag: Option<&str>) -> Result<()> {
        self.delete_object(href, credential, etag).await
    }

    /// Stores `body` as an object at `href` via `PUT` with a `Content-Type`
    /// of `content_type` - the shared write verb behind the calendar and
    /// vCard wrappers (both are "put a text document at a collection-relative
    /// href with a precondition guard" under the same error convention).
    /// `etag` is the resource's current `getetag` for an update - sent as
    /// `If-Match`, so a write based on a stale copy fails with HTTP 412 rather
    /// than silently clobbering a concurrent change. `None` marks a create and
    /// sends `If-None-Match: *`, refusing to overwrite an existing resource.
    /// Returns the server's new etag (the `ETag` response header), if any.
    pub async fn put_object(&self, href: &str, body: &str, content_type: &str, credential: &Credential, etag: Option<&str>) -> Result<Option<String>> {
        let url = self.resolve(href)?;
        let request_url = url.clone();
        let mut headers = vec![("Content-Type", content_type.to_string())];
        match etag {
            Some(etag) => headers.push(("If-Match", format!("\"{}\"", etag.trim_matches('"')))),
            None => headers.push(("If-None-Match", "*".to_string())),
        }
        let response = self.send_request("PUT", url, credential, Some(body.to_string()), &headers).await?;
        tracing::debug!("DAV PUT {request_url} -> {}", response.status());
        let new_etag = response.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
        // Drain the body so the connection can be reused.
        drop(response.text().await?);
        Ok(new_etag)
    }

    /// Deletes the object at `href` via `DELETE` - the shared write verb
    /// behind the calendar and vCard wrappers. An optional `etag` is sent as
    /// `If-Match` so deleting a resource that has changed since it was
    /// fetched fails loudly instead of silently removing a concurrent edit.
    pub async fn delete_object(&self, href: &str, credential: &Credential, etag: Option<&str>) -> Result<()> {
        let url = self.resolve(href)?;
        let request_url = url.clone();
        let mut headers: Vec<(&str, String)> = Vec::new();
        if let Some(etag) = etag {
            headers.push(("If-Match", format!("\"{}\"", etag.trim_matches('"'))));
        }
        let response = self.send_request("DELETE", url, credential, None, &headers).await?;
        tracing::debug!("DAV DELETE {request_url} -> {}", response.status());
        drop(response.text().await?);
        Ok(())
    }

    fn resolve(&self, href: &str) -> Result<reqwest::Url> {
        self.base_url.join(href).map_err(|e| Error::Discovery(format!("couldn't resolve href {href:?}: {e}")))
    }

    async fn propfind(&self, url: reqwest::Url, depth: u8, credential: &Credential, props: &[&str]) -> Result<Vec<DavResponse>> {
        self.send_xml_request("PROPFIND", url, depth, credential, xml::build_propfind_body(props)).await
    }

    async fn report(&self, url: reqwest::Url, depth: u8, credential: &Credential, body: String) -> Result<Vec<DavResponse>> {
        self.send_xml_request("REPORT", url, depth, credential, body).await
    }

    /// Sends a `sync-collection` REPORT request, used by CardDAV/other WebDAV
    /// collection sync flows that require RFC 6578 incremental discovery.
    pub async fn sync_collection(&self, url: reqwest::Url, depth: u8, credential: &Credential, body: String) -> Result<Vec<DavResponse>> {
        self.report(url, depth, credential, body).await
    }

    /// Sends `method_name` to `url` with `credential` auth applied and
    /// `extra_headers`/`body` attached, erroring with the server's (truncated)
    /// response body on any non-2xx - the same body-snippet convention every
    /// other DAV method uses, since CalDAV servers explain their 4xxes in it.
    /// Shared by the XML-multistatus methods (which parse the body) and the
    /// calendar write methods (which only need status/headers).
    async fn send_request(
        &self,
        method_name: &str,
        url: reqwest::Url,
        credential: &Credential,
        body: Option<String>,
        extra_headers: &[(&str, String)],
    ) -> Result<reqwest::Response> {
        let method = Method::from_bytes(method_name.as_bytes()).expect("method name is a valid HTTP token");
        let request_url = url.clone();
        tracing::debug!("DAV {method_name} {request_url} body:\n{:?}", body);
        let mut req = self.http.request(method, url);
        for (name, value) in extra_headers {
            req = req.header(*name, value);
        }
        if let Some(body) = body {
            req = req.body(body);
        }
        req = match credential {
            // CalDAV's actual auth mechanism is plain HTTP headers, not the
            // SASL-inside-IMAP `AUTHENTICATE XOAUTH2` Mail uses - nothing to
            // share between the two crates here.
            Credential::Password(password) => req.basic_auth(&self.username, Some(password)),
            Credential::OAuth2AccessToken(token) => req.bearer_auth(token),
        };

        let response = req.send().await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await?;
            tracing::debug!("DAV {method_name} {request_url} -> {status}, body:\n{text}");
            // `error_for_status()` would discard the response body here -
            // and CardDAV/CalDAV servers (Google, Nextcloud, ...) routinely
            // explain exactly what they didn't like about the request in it,
            // which is far more useful for diagnosing a 4xx than a bare
            // "400 Bad Request" with no context. The body is HTML and
            // control-character-laden, so it's sanitized before it can land
            // in a GTK toast or label.
            let snippet = sanitize_snippet(&text);
            return Err(Error::Discovery(format!("HTTP {status} for {request_url}: {snippet}")));
        }
        Ok(response)
    }

    async fn send_xml_request(&self, method_name: &str, url: reqwest::Url, depth: u8, credential: &Credential, body: String) -> Result<Vec<DavResponse>> {
        let request_url = url.clone();
        let response = self
            .send_request(
                method_name,
                url,
                credential,
                Some(body),
                &[("Depth", depth.to_string()), ("Content-Type", "application/xml; charset=utf-8".to_string())],
            )
            .await?;
        let status = response.status();
        let text = response.text().await?;
        tracing::debug!("DAV {method_name} {request_url} -> {status}, body:\n{text}");
        xml::parse_multistatus(&text)
    }

    /// [`Self::send_xml_request`] but also returns the top-level
    /// `<sync-token>` (RFC 6578) when the response carries one - the
    /// incremental-sync path.
    async fn send_xml_request_with_token(
        &self,
        method_name: &str,
        url: reqwest::Url,
        depth: u8,
        credential: &Credential,
        body: String,
    ) -> Result<(Vec<DavResponse>, Option<String>)> {
        let request_url = url.clone();
        let response = self
            .send_request(
                method_name,
                url,
                credential,
                Some(body),
                &[("Depth", depth.to_string()), ("Content-Type", "application/xml; charset=utf-8".to_string())],
            )
            .await?;
        let status = response.status();
        let text = response.text().await?;
        tracing::debug!("DAV {method_name} {request_url} -> {status}, body:\n{text}");
        xml::parse_multistatus_with_token(&text)
    }
}

#[cfg(test)]
mod tests {
    use lookout_core::AccountId;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn normalize_webcal_url_rewrites_webcal_schemes_and_keeps_http_https() {
        assert_eq!(normalize_webcal_url("webcal://example.com/feed.ics").unwrap().as_str(), "http://example.com/feed.ics");
        assert_eq!(normalize_webcal_url("webcals://example.com/feed.ics").unwrap().as_str(), "https://example.com/feed.ics");
        // Scheme matching is case-insensitive; https passes through untouched.
        assert_eq!(normalize_webcal_url("WEBCAL://example.com/feed.ics").unwrap().as_str(), "http://example.com/feed.ics");
        assert_eq!(normalize_webcal_url("https://example.com/feed.ics").unwrap().as_str(), "https://example.com/feed.ics");
        // Surrounding whitespace is tolerated (users paste URLs).
        assert_eq!(normalize_webcal_url("  http://example.com/x.ics  ").unwrap().as_str(), "http://example.com/x.ics");
    }

    #[test]
    fn normalize_webcal_url_rejects_garbage_and_unsupported_schemes() {
        assert!(normalize_webcal_url("not a url").is_err());
        assert!(normalize_webcal_url("").is_err());
        let err = normalize_webcal_url("ftp://example.com/feed.ics").unwrap_err();
        assert!(err.to_string().contains("unsupported scheme"), "{err}");
    }

    #[tokio::test]
    async fn fetch_webcal_ics_returns_the_feed_body() {
        let server = MockServer::start().await;
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n";
        Mock::given(method("GET"))
            .and(path("/feed.ics"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ics))
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let url = format!("{}/feed.ics", server.uri()).parse().unwrap();
        assert_eq!(fetch_webcal_ics(&http, &url).await.unwrap(), ics);
    }

    #[tokio::test]
    async fn fetch_webcal_ics_surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing.ics"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not here"))
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let url = format!("{}/missing.ics", server.uri()).parse().unwrap();
        let err = fetch_webcal_ics(&http, &url).await.unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[tokio::test]
    async fn fetch_webcal_ics_rejects_bodies_over_the_size_limit() {
        let server = MockServer::start().await;
        let huge = "x".repeat(MAX_FEED_BYTES + 1);
        Mock::given(method("GET"))
            .and(path("/huge.ics"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&huge))
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let url = format!("{}/huge.ics", server.uri()).parse().unwrap();
        let err = fetch_webcal_ics(&http, &url).await.unwrap_err();
        assert!(err.to_string().contains("size limit"), "{err}");
    }

    #[tokio::test]
    async fn discovers_home_lists_calendars_and_fetches_events_end_to_end() {
        let server = MockServer::start().await;

        Mock::given(method("PROPFIND"))
            .and(path("/dav/"))
            .respond_with(ResponseTemplate::new(207).insert_header("Content-Type", "application/xml").set_body_string(
                r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/dav/</D:href>
    <D:propstat>
      <D:prop><D:current-user-principal><D:href>/principals/alice/</D:href></D:current-user-principal></D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#,
            ))
            .mount(&server)
            .await;

        Mock::given(method("PROPFIND"))
            .and(path("/principals/alice/"))
            .respond_with(ResponseTemplate::new(207).insert_header("Content-Type", "application/xml").set_body_string(
                r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/principals/alice/</D:href>
    <D:propstat>
      <D:status>HTTP/1.1 200 OK</D:status>
      <D:prop>
        <C:calendar-home-set>
          <D:href>/calendars/alice/home/</D:href>
        </C:calendar-home-set>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#,
            ))
            .mount(&server)
            .await;

        Mock::given(method("PROPFIND"))
            .and(path("/calendars/alice/home/"))
            .respond_with(ResponseTemplate::new(207).insert_header("Content-Type", "application/xml").set_body_string(
                r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/calendars/alice/home/</D:href>
    <D:propstat>
      <D:status>HTTP/1.1 200 OK</D:status>
      <D:prop>
        <D:resourcetype>
          <D:collection/>
        </D:resourcetype>
      </D:prop>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/calendars/alice/home/personal/</D:href>
    <D:propstat>
      <D:status>HTTP/1.1 200 OK</D:status>
      <D:prop>
        <D:displayname>Personal</D:displayname>
        <D:resourcetype>
          <D:collection/>
          <C:calendar/>
        </D:resourcetype>
        <C:supported-calendar-component-set>
          <C:comp name="VEVENT"/>
          <C:comp name="VTODO"/>
        </C:supported-calendar-component-set>
      </D:prop>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/calendars/alice/home/holidays/</D:href>
    <D:propstat>
      <D:status>HTTP/1.1 200 OK</D:status>
      <D:prop>
        <D:displayname>Holidays (read-only)</D:displayname>
        <D:resourcetype>
          <D:collection/>
          <C:calendar/>
        </D:resourcetype>
        <C:supported-calendar-component-set>
          <C:comp name="VEVENT"/>
        </C:supported-calendar-component-set>
      </D:prop>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/calendars/alice/home/no-advert/</D:href>
    <D:propstat>
      <D:status>HTTP/1.1 200 OK</D:status>
      <D:prop>
        <D:displayname>No advert</D:displayname>
        <D:resourcetype>
          <D:collection/>
          <C:calendar/>
        </D:resourcetype>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#,
            ))
            .mount(&server)
            .await;

        Mock::given(method("REPORT"))
            .and(path("/calendars/alice/home/personal/"))
            .respond_with(
                ResponseTemplate::new(207).insert_header("Content-Type", "application/xml").set_body_string(
                    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">\n  <D:response>\n    <D:href>/calendars/alice/home/personal/event1.ics</D:href>\n    <D:propstat>\n      <D:prop>\n        <D:getetag>\"abc123\"</D:getetag>\n        <C:calendar-data>BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:evt-1@example.com\r\nDTSTART:20260710T090000Z\r\nDTEND:20260710T100000Z\r\nSUMMARY:Team sync\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n</C:calendar-data>\n      </D:prop>\n      <D:status>HTTP/1.1 200 OK</D:status>\n    </D:propstat>\n  </D:response>\n</D:multistatus>",
                ),
            )
            .mount(&server)
            .await;

        let base_url = format!("{}/dav/", server.uri());
        let client = DavClient::new(&base_url, false, "alice".to_string()).unwrap();
        let credential = Credential::Password("secret".to_string());

        let home_href = client.discover_calendar_home(&credential).await.unwrap();
        assert_eq!(home_href, "/calendars/alice/home/");

        let account_id = AccountId("test-account".to_string());
        let calendars = client.list_calendars(&home_href, &account_id, &credential).await.unwrap();
        assert_eq!(calendars.len(), 3, "the home collection itself must not be mistaken for a calendar");
        assert_eq!(calendars[0].display_name, "Personal");
        assert_eq!(calendars[0].href, "/calendars/alice/home/personal/");
        assert!(calendars[0].supports_tasks, "a VEVENT+VTODO component set advertises task support");
        assert!(!calendars[1].supports_tasks, "a VEVENT-only component set (Google-style) must not offer tasks");
        assert!(calendars[2].supports_tasks, "a missing component set is assumed to support tasks, not refused");

        // The task fetch only runs against task-capable calendars.
        let personal = calendars.iter().find(|c| c.href.ends_with("/personal/")).unwrap();
        let tasks = client.fetch_tasks(personal, &credential).await.unwrap();
        assert!(tasks.is_empty(), "the personal fixture has no VTODO resources");

        let start = "2026-07-01T00:00:00Z".parse().unwrap();
        let end = "2026-08-01T00:00:00Z".parse().unwrap();
        let events = client.fetch_events_in_range(&calendars[0], start, end, &credential).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid.0, "evt-1@example.com");
        assert_eq!(events[0].summary.as_deref(), Some("Team sync"));
        // The multistatus `<href>`/`<getetag>` must have been stamped onto the
        // event - the write path needs both to PUT/DELETE it back.
        assert_eq!(events[0].href.as_deref(), Some("/calendars/alice/home/personal/event1.ics"));
        assert_eq!(events[0].etag.as_deref(), Some("\"abc123\""));
    }

    #[tokio::test]
    async fn put_calendar_object_creates_with_if_none_match_and_returns_new_etag() {
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/cal/events/new-event.ics"))
            .and(wiremock::matchers::header("If-None-Match", "*"))
            .and(wiremock::matchers::body_string_contains("SUMMARY:Created event"))
            .respond_with(ResponseTemplate::new(201).insert_header("ETag", "\"etag-new\""))
            .mount(&server)
            .await;

        let base_url = format!("{}/dav/", server.uri());
        let client = DavClient::new(&base_url, false, "alice".to_string()).unwrap();
        let credential = Credential::Password("secret".to_string());

        let etag = client
            .put_calendar_object(
                "/cal/events/new-event.ics",
                "BEGIN:VCALENDAR\r\nSUMMARY:Created event\r\nEND:VCALENDAR\r\n",
                &credential,
                None,
            )
            .await
            .unwrap();
        assert_eq!(etag.as_deref(), Some("\"etag-new\""));
    }

    #[tokio::test]
    async fn put_calendar_object_updates_with_if_match_and_normalizes_the_etag_quotes() {
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/cal/events/evt.ics"))
            .and(wiremock::matchers::header("If-Match", "\"old-etag\""))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let base_url = format!("{}/dav/", server.uri());
        let client = DavClient::new(&base_url, false, "alice".to_string()).unwrap();
        let credential = Credential::Password("secret".to_string());

        // An unquoted etag is normalized to the quoted form `If-Match` wants.
        client
            .put_calendar_object("/cal/events/evt.ics", "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n", &credential, Some("old-etag"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn put_calendar_object_412_surfaces_the_server_body_snippet() {
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(412).set_body_string("Precondition Failed: etag mismatch on server"))
            .mount(&server)
            .await;

        let base_url = format!("{}/dav/", server.uri());
        let client = DavClient::new(&base_url, false, "alice".to_string()).unwrap();
        let credential = Credential::Password("secret".to_string());

        let err = client
            .put_calendar_object("/cal/events/evt.ics", "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n", &credential, Some("stale-etag"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("412"), "error should surface the HTTP status: {err}");
        assert!(err.to_string().contains("etag mismatch"), "error should include the server's explanation: {err}");
    }

    #[tokio::test]
    async fn delete_calendar_object_sends_if_match_and_succeeds() {
        let server = MockServer::start().await;

        Mock::given(method("DELETE"))
            .and(path("/cal/events/evt.ics"))
            .and(wiremock::matchers::header("If-Match", "\"etag-del\""))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let base_url = format!("{}/dav/", server.uri());
        let client = DavClient::new(&base_url, false, "alice".to_string()).unwrap();
        let credential = Credential::Password("secret".to_string());

        client.delete_calendar_object("/cal/events/evt.ics", &credential, Some("\"etag-del\"")).await.unwrap();
    }

    fn sample_card() -> VCard {
        VCard {
            version: "4.0".to_string(),
            kind: None,
            uid: Some("c1@example.com".to_string()),
            full_name: Some("Jane Doe".to_string()),
            name: None,
            organization: None,
            title: None,
            emails: vec![lookout_core::EmailField {
                types: vec!["work".to_string()],
                address: "jane@example.com".to_string(),
            }],
            telephones: Vec::new(),
            addresses: Vec::new(),
            urls: Vec::new(),
            note: None,
            birthday: None,
            categories: Vec::new(),
            other: Vec::new(),
        }
    }

    #[tokio::test]
    async fn put_contact_vcard_creates_with_if_none_match_and_vcard_content_type() {
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/books/alice/jane.vcf"))
            .and(wiremock::matchers::header("If-None-Match", "*"))
            .and(wiremock::matchers::header("Content-Type", "text/vcard; charset=utf-8"))
            .and(wiremock::matchers::body_string_contains("FN:Jane Doe"))
            .respond_with(ResponseTemplate::new(201).insert_header("ETag", "\"etag-new\""))
            .mount(&server)
            .await;

        let base_url = format!("{}/dav/", server.uri());
        let client = DavClient::new(&base_url, false, "alice".to_string()).unwrap();
        let credential = Credential::Password("secret".to_string());

        let etag = client.put_contact_vcard("/books/alice/jane.vcf", &sample_card(), &credential, None).await.unwrap();
        assert_eq!(etag.as_deref(), Some("\"etag-new\""));
    }

    #[tokio::test]
    async fn put_contact_vcard_updates_with_if_match() {
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/books/alice/jane.vcf"))
            .and(wiremock::matchers::header("If-Match", "\"old-etag\""))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let base_url = format!("{}/dav/", server.uri());
        let client = DavClient::new(&base_url, false, "alice".to_string()).unwrap();
        let credential = Credential::Password("secret".to_string());

        // An unquoted etag is normalized to the quoted form `If-Match` wants.
        client
            .put_contact_vcard("/books/alice/jane.vcf", &sample_card(), &credential, Some("old-etag"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn put_contact_vcard_412_surfaces_the_server_body_snippet() {
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(412).set_body_string("Precondition Failed: stale etag"))
            .mount(&server)
            .await;

        let base_url = format!("{}/dav/", server.uri());
        let client = DavClient::new(&base_url, false, "alice".to_string()).unwrap();
        let credential = Credential::Password("secret".to_string());

        let err = client
            .put_contact_vcard("/books/alice/jane.vcf", &sample_card(), &credential, Some("stale-etag"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("412"), "error should surface the HTTP status: {err}");
        assert!(err.to_string().contains("stale etag"), "error should include the server's explanation: {err}");
    }

    #[tokio::test]
    async fn delete_contact_vcard_sends_if_match_and_succeeds() {
        let server = MockServer::start().await;

        Mock::given(method("DELETE"))
            .and(path("/books/alice/jane.vcf"))
            .and(wiremock::matchers::header("If-Match", "\"etag-del\""))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let base_url = format!("{}/dav/", server.uri());
        let client = DavClient::new(&base_url, false, "alice".to_string()).unwrap();
        let credential = Credential::Password("secret".to_string());

        client.delete_contact_vcard("/books/alice/jane.vcf", &credential, Some("\"etag-del\"")).await.unwrap();
    }

    #[test]
    fn new_strips_userinfo_from_base_url() {
        let client = DavClient::new("https://alice@caldav.example.com/remote.php/dav/", false, "alice".to_string()).unwrap();
        assert_eq!(client.base_url.to_string(), "https://caldav.example.com/remote.php/dav/");
    }

    #[test]
    fn sanitize_snippet_cleans_nextcloud_style_error_page() {
        let raw = "<html>\r\n<head><title>400 Bad Request</title></head>\r\n<body><center><h1>400 Bad Request</h1></center>\r\n<hr><center>nginx</center></body></html>\r\n";
        let clean = sanitize_snippet(raw);
        assert_eq!(clean, " 400 Bad Request 400 Bad Request nginx");
    }

    #[test]
    fn sanitize_snippet_keeps_plain_server_explanations() {
        let clean = sanitize_snippet("Precondition Failed: etag mismatch on server");
        assert_eq!(clean, "Precondition Failed: etag mismatch on server");
    }

    #[test]
    fn sanitize_snippet_drops_control_characters() {
        // `\r` and other control junk are dropped; `\n`/`\t` collapse to a
        // single space along with the rest of the whitespace.
        let clean = sanitize_snippet("line one\r\nline\u{1b}two\u{7f}\ttab");
        assert_eq!(clean, "line one linetwo tab");
    }

    #[test]
    fn sanitize_snippet_handles_unclosed_tag_and_caps_length() {
        // The space around `< b >` is consumed as part of the tag, and the
        // trailing unclosed `<` swallows the rest without panicking.
        assert_eq!(sanitize_snippet("a < b > c <"), "a c");
        assert_eq!(sanitize_snippet("keep <unclosed"), "keep");
        let long = format!("<b>{}</b>", "x".repeat(10_000));
        let clean = sanitize_snippet(&long);
        assert_eq!(clean, "x".repeat(MAX_ERROR_SNIPPET_CHARS), "snippet must be capped at MAX_ERROR_SNIPPET_CHARS");
    }
}
