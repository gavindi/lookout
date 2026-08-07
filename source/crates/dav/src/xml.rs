//! Minimal WebDAV/CalDAV `multistatus` XML support: request-body builders
//! for the handful of PROPFIND/REPORT queries this crate needs, and a
//! namespace-aware parser for the responses.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::reader::NsReader;

use crate::error::Result;

pub const NS_CARDDAV: &[u8] = b"urn:ietf:params:xml:ns:carddav";

/// One `<response>` entry from a `multistatus` document: its href, plus
/// whatever requested properties it reported, keyed by `(namespace, local
/// name)` - resolved via the server's actual namespace URIs, not raw
/// (possibly server-specific) prefix strings, so a server using `xmlns:x1=`
/// instead of the conventional `xmlns:d=` still parses correctly.
#[derive(Debug, Clone, Default)]
pub struct DavResponse {
    pub href: String,
    /// The response-level `<d:status>` when the server reports one as a
    /// direct child of `<response>` (RFC 6578 `sync-collection` marks removed
    /// members this way, e.g. `HTTP/1.1 404 Not Found`, with no `propstat`).
    /// Absent for the common 200-with-props case where the status lives
    /// inside each `<propstat>` instead.
    pub status: Option<String>,
    props: HashMap<(Vec<u8>, String), String>,
    /// The `name` attribute values of `<comp>` children of each prop element,
    /// keyed like `props` - a `supported-calendar-component-set`'s
    /// `VEVENT`/`VTODO`/... list. Empty for every prop without `<comp>`
    /// children (resourcetype and the rest are unaffected).
    comp_sets: HashMap<(Vec<u8>, String), Vec<String>>,
}

impl DavResponse {
    /// Looks up a property by its resolved namespace URI and local name.
    /// For a scalar leaf prop (e.g. `displayname`) this is its text content.
    /// For a container prop with element children instead of text (e.g.
    /// `resourcetype`'s `<collection/>`/`<calendar/>` markers) this is a
    /// comma-joined list of the children's local names - good enough to
    /// test for the presence of a marker (`.contains("calendar")`) without
    /// needing a full XML tree.
    pub fn prop(&self, ns: &[u8], local_name: &str) -> Option<&str> {
        self.props.get(&(ns.to_vec(), local_name.to_string())).map(|s| s.as_str())
    }

    /// The `name` attribute values of a prop's `<comp>` children (e.g. a
    /// `supported-calendar-component-set`'s component list). Empty when the
    /// prop is absent or has no `<comp>` children - callers that need to
    /// distinguish "server didn't advertise" from "advertised, empty" must
    /// treat absence via [`Self::prop`].
    pub fn prop_comps(&self, ns: &[u8], local_name: &str) -> &[String] {
        self.comp_sets.get(&(ns.to_vec(), local_name.to_string())).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Whether the prop declared `<comp name="…"/>` among its children -
    /// e.g. a calendar's `supported-calendar-component-set` containing
    /// `VTODO`.
    pub fn supports_component(&self, ns: &[u8], local_name: &str, comp: &str) -> bool {
        self.prop_comps(ns, local_name).iter().any(|c| c == comp)
    }
}

fn ns_bytes(ns: &ResolveResult) -> Vec<u8> {
    match ns {
        ResolveResult::Bound(Namespace(b)) => b.to_vec(),
        _ => Vec::new(),
    }
}

/// The element's attributes as `(local name, value)` pairs - only the local
/// name matters here (`comp`'s `name` attribute is unqualified in RFC 4791).
fn start_attrs(e: &quick_xml::events::BytesStart) -> Vec<(String, String)> {
    e.attributes()
        .filter_map(|a| a.ok())
        .map(|a| {
            (
                String::from_utf8_lossy(a.key.local_name().as_ref()).to_string(),
                String::from_utf8_lossy(&a.value).to_string(),
            )
        })
        .collect()
}

#[derive(Default)]
struct ParseState {
    responses: Vec<DavResponse>,
    current: Option<DavResponse>,
    response_depth: Option<u32>,
    /// Depth at which a response-level `<status>` element opened (a direct
    /// child of `<response>`), plus its accumulated text. Kept separate from
    /// `prop_depth`'s props because a `propstat`-level status is *not* the
    /// response's status.
    status_depth: Option<u32>,
    status_text: String,
    /// Depth at which the top-level `<sync-token>` element opened (a direct
    /// child of the `<multistatus>` root), plus its accumulated text.
    sync_token_depth: Option<u32>,
    sync_token_text: String,
    // Depth at which a <href> element opened, plus its accumulated text.
    // hrefs appear both as a direct child of <response> (the resource's own
    // href) and, for a couple of props (current-user-principal,
    // calendar-home-set), wrapped one level inside the prop element itself.
    href_open_depth: Option<u32>,
    href_text: String,
    prop_depth: Option<u32>,
    prop_key: Option<(Vec<u8>, String)>,
    prop_text: String,
    prop_child_names: Vec<String>,
    /// `name` attribute values of `<comp>` child elements of the current
    /// prop - the `supported-calendar-component-set`'s component list.
    prop_comp_attrs: Vec<String>,
}

impl ParseState {
    fn on_open(&mut self, depth: u32, ns: &ResolveResult, local: &str, attrs: &[(String, String)]) {
        if local == "response" {
            self.response_depth = Some(depth);
            self.current = Some(DavResponse::default());
            return;
        }
        if local == "href" {
            self.href_open_depth = Some(depth);
            self.href_text.clear();
            return;
        }
        if local == "prop" && self.prop_depth.is_none() {
            self.prop_depth = Some(depth);
            return;
        }
        // Response-level status (direct child of <response>): depth 2 for
        // <multistatus><response><status>. Root is at depth 1.
        if local == "status" && self.response_depth == Some(depth - 1) {
            self.status_depth = Some(depth);
            self.status_text.clear();
            return;
        }
        // Top-level sync token (direct child of the <multistatus> root, RFC
        // 6578): depth 2.
        if local == "sync-token" && depth == 2 {
            self.sync_token_depth = Some(depth);
            self.sync_token_text.clear();
            return;
        }
        if let Some(pd) = self.prop_depth {
            if depth == pd + 1 {
                self.prop_key = Some((ns_bytes(ns), local.to_string()));
                self.prop_text.clear();
                self.prop_child_names.clear();
                self.prop_comp_attrs.clear();
            } else if depth > pd + 1 {
                self.prop_child_names.push(local.to_string());
                if local == "comp" {
                    if let Some((_, value)) = attrs.iter().find(|(key, _)| key == "name") {
                        self.prop_comp_attrs.push(value.clone());
                    }
                }
            }
        }
    }

    fn on_text(&mut self, depth: u32, text: &str) {
        if self.href_open_depth == Some(depth) {
            self.href_text.push_str(text);
        } else if self.status_depth == Some(depth) {
            self.status_text.push_str(text);
        } else if self.sync_token_depth == Some(depth) {
            self.sync_token_text.push_str(text);
        } else if let Some(pd) = self.prop_depth {
            if depth == pd + 1 && self.prop_key.is_some() {
                self.prop_text.push_str(text);
            }
        }
    }

    fn on_close(&mut self, depth: u32) {
        if self.status_depth == Some(depth) {
            if let Some(resp) = self.current.as_mut() {
                resp.status = Some(self.status_text.trim().to_string());
            }
            self.status_depth = None;
            return;
        }
        if self.sync_token_depth == Some(depth) {
            self.sync_token_text = self.sync_token_text.trim().to_string();
            self.sync_token_depth = None;
            return;
        }
        if self.href_open_depth == Some(depth) {
            let href_value = self.href_text.trim().to_string();
            if self.response_depth == Some(depth - 1) {
                // Direct child of <response> - the resource's own href.
                if let Some(resp) = self.current.as_mut() {
                    resp.href = href_value;
                }
            } else if let Some(pd) = self.prop_depth {
                if depth - 1 == pd + 1 {
                    // Wrapped one level inside a leaf prop element, e.g.
                    // <current-user-principal><href>...</href></current-user-principal>.
                    // Its value *is* the enclosing prop's value.
                    self.prop_text = href_value;
                }
            }
            self.href_open_depth = None;
            return;
        }

        if let Some(pd) = self.prop_depth {
            if depth == pd + 1 {
                if let (Some(key), Some(resp)) = (self.prop_key.take(), self.current.as_mut()) {
                    // Whitespace between child elements (pretty-printed XML,
                    // e.g. Google's `calendar-home-set` href or a multi-line
                    // `resourcetype`) leaks into `prop_text` - treat
                    // whitespace-only text as "no text", and trim the final
                    // value, so formatting never contaminates the result.
                    let has_text = !self.prop_text.trim().is_empty();
                    let value = if !self.prop_child_names.is_empty() && !has_text {
                        self.prop_child_names.join(",")
                    } else {
                        self.prop_text.trim().to_string()
                    };
                    resp.props.insert(key.clone(), value);
                    if !self.prop_comp_attrs.is_empty() {
                        resp.comp_sets.insert(key, std::mem::take(&mut self.prop_comp_attrs));
                    }
                }
            } else if depth == pd {
                self.prop_depth = None;
            }
        }
        if self.response_depth == Some(depth) {
            if let Some(resp) = self.current.take() {
                self.responses.push(resp);
            }
            self.response_depth = None;
        }
    }
}

/// Parses a WebDAV/CalDAV `multistatus` response body into one [`DavResponse`]
/// per `<response>` element.
pub fn parse_multistatus(xml: &str) -> Result<Vec<DavResponse>> {
    let mut reader = NsReader::from_str(xml);
    // Deliberately NOT trim_text(true): that trims *each* Text event
    // individually, which mangles content split around an entity reference
    // (e.g. "Work &amp; Meetings" arrives as three events - "Work ", the
    // entity, " Meetings" - and per-event trimming eats the meaningful
    // spaces). Trimming happens once, on the fully-assembled value, in
    // `ParseState::on_close` instead.

    let mut state = ParseState::default();
    let mut depth: u32 = 0;

    loop {
        let (ns, event) = reader.read_resolved_event()?;
        match event {
            Event::Eof => break,
            Event::Start(e) => {
                depth += 1;
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                state.on_open(depth, &ns, &local, &start_attrs(&e));
            }
            Event::Empty(e) => {
                depth += 1;
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                state.on_open(depth, &ns, &local, &start_attrs(&e));
                state.on_close(depth);
                depth -= 1;
            }
            Event::Text(t) => {
                let decoded = t.decode().map_err(quick_xml::Error::from)?;
                state.on_text(depth, &decoded);
            }
            Event::GeneralRef(r) => {
                // `BytesRef::xml10_content()` only decodes bytes, it doesn't
                // resolve entities - numeric char refs need
                // `resolve_char_ref()`, and XML core only predefines 5 named
                // entities (no DTD support here, so nothing else is legal).
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => Some(c.to_string()),
                    Ok(None) => r.decode().ok().and_then(|name| match name.as_ref() {
                        "amp" => Some("&".to_string()),
                        "lt" => Some("<".to_string()),
                        "gt" => Some(">".to_string()),
                        "apos" => Some("'".to_string()),
                        "quot" => Some("\"".to_string()),
                        _ => None,
                    }),
                    Err(_) => None,
                };
                if let Some(resolved) = resolved {
                    state.on_text(depth, &resolved);
                }
            }
            Event::End(_) => {
                state.on_close(depth);
                depth -= 1;
            }
            _ => {}
        }
    }

    Ok(state.responses)
}

/// Like [`parse_multistatus`], but also returns the top-level `<sync-token>`
/// (RFC 6578) if the server included one - the incremental-sync cursor the
/// caller must store and send back on the next `sync-collection` REPORT.
pub fn parse_multistatus_with_token(xml: &str) -> Result<(Vec<DavResponse>, Option<String>)> {
    let mut reader = NsReader::from_str(xml);
    let mut state = ParseState::default();
    let mut depth: u32 = 0;

    loop {
        let (ns, event) = reader.read_resolved_event()?;
        match event {
            Event::Eof => break,
            Event::Start(e) => {
                depth += 1;
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                state.on_open(depth, &ns, &local, &start_attrs(&e));
            }
            Event::Empty(e) => {
                depth += 1;
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                state.on_open(depth, &ns, &local, &start_attrs(&e));
                state.on_close(depth);
                depth -= 1;
            }
            Event::Text(t) => {
                let decoded = t.decode().map_err(quick_xml::Error::from)?;
                state.on_text(depth, &decoded);
            }
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => Some(c.to_string()),
                    Ok(None) => r.decode().ok().and_then(|name| match name.as_ref() {
                        "amp" => Some("&".to_string()),
                        "lt" => Some("<".to_string()),
                        "gt" => Some(">".to_string()),
                        "apos" => Some("'".to_string()),
                        "quot" => Some("\"".to_string()),
                        _ => None,
                    }),
                    Err(_) => None,
                };
                if let Some(resolved) = resolved {
                    state.on_text(depth, &resolved);
                }
            }
            Event::End(_) => {
                state.on_close(depth);
                depth -= 1;
            }
            _ => {}
        }
    }

    Ok((state.responses, (!state.sync_token_text.is_empty()).then_some(state.sync_token_text)))
}

/// Builds a `PROPFIND` request body asking for the given (already-prefixed)
/// property names, e.g. `&["D:current-user-principal"]` or
/// `&["D:displayname", "D:resourcetype", "IC:calendar-color"]`.
pub fn build_propfind_body(props: &[&str]) -> String {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n\
         <D:propfind xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CD=\"urn:ietf:params:xml:ns:carddav\" xmlns:IC=\"http://apple.com/ns/ical/\">\n  <D:prop>\n",
    );
    for p in props {
        body.push_str("    <");
        body.push_str(p);
        body.push_str("/>\n");
    }
    body.push_str("  </D:prop>\n</D:propfind>");
    body
}

/// Builds a `calendar-query` REPORT body requesting `VEVENT`s overlapping
/// `[start, end)`.
pub fn build_calendar_query_body(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n\
         <C:calendar-query xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">\n\
         \x20 <D:prop>\n\
         \x20   <D:getetag/>\n\
         \x20   <C:calendar-data/>\n\
         \x20 </D:prop>\n\
         \x20 <C:filter>\n\
         \x20   <C:comp-filter name=\"VCALENDAR\">\n\
         \x20     <C:comp-filter name=\"VEVENT\">\n\
         \x20       <C:time-range start=\"{}\" end=\"{}\"/>\n\
         \x20     </C:comp-filter>\n\
         \x20   </C:comp-filter>\n\
         \x20 </C:filter>\n\
         </C:calendar-query>",
        start.format("%Y%m%dT%H%M%SZ"),
        end.format("%Y%m%dT%H%M%SZ"),
    )
}

/// Builds a `todo-query` REPORT body (RFC 4791 §7.10) requesting every
/// `VTODO` in the collection. Unlike the event query there's deliberately no
/// `time-range` filter: tasks carry no required temporal span (a task may
/// have only a `DUE`, only a `DTSTART`, or neither), so a windowed fetch
/// could silently miss them - they're small, and the whole set is re-fetched
/// per poll.
pub fn build_todo_query_body() -> String {
    "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n\
     <C:todo-query xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">\n\
     \x20 <D:prop>\n\
     \x20   <D:getetag/>\n\
     \x20   <C:calendar-data/>\n\
     \x20 </D:prop>\n\
     \x20 <C:filter>\n\
     \x20   <C:comp-filter name=\"VCALENDAR\">\n\
     \x20     <C:comp-filter name=\"VTODO\"/>\n\
     \x20   </C:comp-filter>\n\
     \x20 </C:filter>\n\
     </C:todo-query>"
        .to_string()
}

/// Builds an `addressbook-multiget` REPORT body (RFC 6352 §8.7) requesting
/// the given, already-known member hrefs of a CardDAV collection. Used for
/// a full fetch after a `PROPFIND` (Depth: 1) enumerates the collection's
/// members - deliberately not `addressbook-query`'s filter mechanism (RFC
/// 6352 §8.6), whose "match everything" idiom is an empty `<CD:filter/>` per
/// the RFC's own example, but at least one real-world server (Google's
/// CardDAV) instead treats a filter with no `prop-filter` children as
/// matching *nothing* - confirmed by it returning a bare, response-less
/// `<multistatus/>` for an account known to have contacts. Multiget carries
/// no such filter-semantics ambiguity: it just asks for specific resources by
/// name.
pub fn build_addressbook_multiget_body(hrefs: &[String], props: &[&str]) -> String {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n\
         <CD:addressbook-multiget xmlns:D=\"DAV:\" xmlns:CD=\"urn:ietf:params:xml:ns:carddav\">\n  <D:prop>\n",
    );
    for p in props {
        body.push_str("    <");
        body.push_str(p);
        body.push_str("/>\n");
    }
    body.push_str("  </D:prop>\n");
    for href in hrefs {
        body.push_str("  <D:href>");
        body.push_str(&escape_text(href));
        body.push_str("</D:href>\n");
    }
    body.push_str("</CD:addressbook-multiget>");
    body
}

/// Minimal XML text-content escaping for values interpolated into a request
/// body (hrefs, sync tokens) - CardDAV hrefs are normally already
/// percent-encoded and safe, but this is cheap insurance against a server
/// handing back a raw `&`/`<`/`>` that would otherwise produce malformed XML.
fn escape_text(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Builds a `sync-collection` REPORT body for WebDAV collections such as
/// CardDAV address books. The caller controls which props are requested and
/// may pass an optional sync token for incremental sync.
pub fn build_sync_collection_body(sync_token: Option<&str>, props: &[&str]) -> String {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n\
         <D:sync-collection xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CD=\"urn:ietf:params:xml:ns:carddav\" xmlns:IC=\"http://apple.com/ns/ical/\">\n",
    );
    // RFC 6578 requires `sync-token` to be present even for the initial sync
    // request - empty, meaning "start from scratch" - rather than omitted.
    // At least one real-world server (Google's CardDAV) treats an omitted
    // element as a malformed request instead of inferring an initial sync.
    match sync_token {
        Some(token) => {
            body.push_str("  <D:sync-token>");
            body.push_str(token);
            body.push_str("</D:sync-token>\n");
        }
        None => body.push_str("  <D:sync-token/>\n"),
    }
    body.push_str("  <D:sync-level>1</D:sync-level>\n");
    body.push_str("  <D:prop>\n");
    for p in props {
        body.push_str("    <");
        body.push_str(p);
        body.push_str("/>\n");
    }
    body.push_str("  </D:prop>\n</D:sync-collection>");
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALENDAR_HOME_MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/principals/users/alice/</D:href>
    <D:propstat>
      <D:prop>
        <D:current-user-principal><D:href>/principals/users/alice/</D:href></D:current-user-principal>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

    // Deliberately uses different namespace prefixes ("d:"/"cal:"/"x1:") than
    // the fixture above, to confirm namespace-URI resolution (not raw prefix
    // matching) is what actually drives parsing.
    const CALENDAR_LIST_MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:x1="http://apple.com/ns/ical/">
  <d:response>
    <d:href>/calendars/alice/home/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/calendars/alice/personal/</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>Personal</d:displayname>
        <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
        <x1:calendar-color>#3584e4FF</x1:calendar-color>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/calendars/alice/work/</d:href>
    <d:propstat>
      <d:prop>
        <d:displayname>Work &amp; Meetings</d:displayname>
        <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    const CALENDAR_QUERY_REPORT: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">\n  <D:response>\n    <D:href>/calendars/alice/personal/event1.ics</D:href>\n    <D:propstat>\n      <D:prop>\n        <D:getetag>\"abc123\"</D:getetag>\n        <C:calendar-data>BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:evt-1@example.com\r\nDTSTART:20260710T090000Z\r\nDTEND:20260710T100000Z\r\nSUMMARY:Team sync\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n</C:calendar-data>\n      </D:prop>\n      <D:status>HTTP/1.1 200 OK</D:status>\n    </D:propstat>\n  </D:response>\n</D:multistatus>";

    const DAV: &[u8] = b"DAV:";
    const CALDAV: &[u8] = b"urn:ietf:params:xml:ns:caldav";
    const APPLE_ICAL: &[u8] = b"http://apple.com/ns/ical/";

    #[test]
    fn parses_wrapped_href_prop_value() {
        let responses = parse_multistatus(CALENDAR_HOME_MULTISTATUS).unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].href, "/principals/users/alice/");
        assert_eq!(responses[0].prop(DAV, "current-user-principal"), Some("/principals/users/alice/"));
    }

    #[test]
    fn resolves_namespaces_regardless_of_prefix_and_detects_calendar_collections() {
        let responses = parse_multistatus(CALENDAR_LIST_MULTISTATUS).unwrap();
        assert_eq!(responses.len(), 3);

        assert_eq!(responses[0].href, "/calendars/alice/home/");
        assert!(!responses[0].prop(DAV, "resourcetype").unwrap().contains("calendar"));

        assert_eq!(responses[1].href, "/calendars/alice/personal/");
        assert_eq!(responses[1].prop(DAV, "displayname"), Some("Personal"));
        assert!(responses[1].prop(DAV, "resourcetype").unwrap().contains("calendar"));
        assert_eq!(responses[1].prop(APPLE_ICAL, "calendar-color"), Some("#3584e4FF"));

        assert_eq!(responses[2].href, "/calendars/alice/work/");
        // Entity-decoded, with surrounding spaces intact.
        assert_eq!(responses[2].prop(DAV, "displayname"), Some("Work & Meetings"));
        assert!(responses[2].prop(DAV, "resourcetype").unwrap().contains("calendar"));
    }

    #[test]
    fn parses_calendar_data_with_embedded_crlf_ics() {
        let responses = parse_multistatus(CALENDAR_QUERY_REPORT).unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].prop(DAV, "getetag"), Some("\"abc123\""));
        let ical = responses[0].prop(CALDAV, "calendar-data").unwrap();
        assert!(ical.contains("UID:evt-1@example.com"));
        assert!(ical.contains("SUMMARY:Team sync"));
    }

    #[test]
    fn parses_supported_calendar_component_set_comp_names() {
        // A Nextcloud-style calendar: VEVENT + VTODO.
        let both = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
            <D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">\n\
              <D:response>\n\
                <D:href>/cal/</D:href>\n\
                <D:propstat>\n\
                  <D:prop>\n\
                    <C:supported-calendar-component-set>\n\
                      <C:comp name=\"VEVENT\"/>\n\
                      <C:comp name=\"VTODO\"/>\n\
                    </C:supported-calendar-component-set>\n\
                  </D:prop>\n\
                  <D:status>HTTP/1.1 200 OK</D:status>\n\
                </D:propstat>\n\
              </D:response>\n\
            </D:multistatus>";
        let responses = parse_multistatus(both).unwrap();
        assert_eq!(
            responses[0].prop_comps(CALDAV, "supported-calendar-component-set"),
            ["VEVENT".to_string(), "VTODO".to_string()]
        );
        assert!(responses[0].supports_component(CALDAV, "supported-calendar-component-set", "VTODO"));
        assert!(responses[0].supports_component(CALDAV, "supported-calendar-component-set", "VEVENT"));

        // A Google-style calendar: VEVENT only.
        let events_only = both.replace("<C:comp name=\"VTODO\"/>\n", "");
        let responses = parse_multistatus(&events_only).unwrap();
        assert!(responses[0].supports_component(CALDAV, "supported-calendar-component-set", "VEVENT"));
        assert!(!responses[0].supports_component(CALDAV, "supported-calendar-component-set", "VTODO"));

        // A non-compliant server that omits the property entirely.
        let absent = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
            <D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">\n\
              <D:response>\n\
                <D:href>/cal/</D:href>\n\
                <D:propstat>\n\
                  <D:prop>\n\
                    <D:displayname>Personal</D:displayname>\n\
                  </D:prop>\n\
                  <D:status>HTTP/1.1 200 OK</D:status>\n\
                </D:propstat>\n\
              </D:response>\n\
            </D:multistatus>";
        let responses = parse_multistatus(absent).unwrap();
        assert!(responses[0].prop_comps(CALDAV, "supported-calendar-component-set").is_empty());
        assert!(!responses[0].supports_component(CALDAV, "supported-calendar-component-set", "VTODO"));
    }

    // Real servers pretty-print their multistatus XML - Google in particular
    // puts the calendar-home-set's `<D:href>` and each resourcetype marker on
    // their own indented line. Formatting whitespace must not leak into the
    // extracted property values (this previously appended "\n    " to the
    // home-set href and replaced resourcetype's "collection,calendar" with
    // whitespace, yielding an empty calendar list for Google accounts).
    const GOOGLE_STYLE_HOME_MULTISTATUS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:caldav="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ical="http://apple.com/ns/ical/">
 <D:response xmlns:carddav="urn:ietf:params:xml:ns:carddav" xmlns:cm="http://cal.me.com/_namespace/" xmlns:md="urn:mobileme:davservices">
  <D:href>/caldav/v2/gavindi@gmail.com/user</D:href>
  <D:propstat>
   <D:status>HTTP/1.1 200 OK</D:status>
   <D:prop>
    <caldav:calendar-home-set>
     <D:href>/caldav/v2/gavindi%40gmail.com/</D:href>
    </caldav:calendar-home-set>
   </D:prop>
  </D:propstat>
 </D:response>
</D:multistatus>"#;

    const GOOGLE_STYLE_LIST_MULTISTATUS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:caldav="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ical="http://apple.com/ns/ical/">
 <D:response xmlns:carddav="urn:ietf:params:xml:ns:carddav" xmlns:cm="http://cal.me.com/_namespace/" xmlns:md="urn:mobileme:davservices">
  <D:href>/caldav/v2/gavindi%40gmail.com/events/</D:href>
  <D:propstat>
   <D:status>HTTP/1.1 200 OK</D:status>
   <D:prop>
    <D:displayname>gavindi@gmail.com</D:displayname>
    <ical:calendar-color>#9FE1E7FF</ical:calendar-color>
    <D:resourcetype>
     <D:collection/>
     <caldav:calendar/>
    </D:resourcetype>
   </D:prop>
  </D:propstat>
 </D:response>
 <D:response xmlns:carddav="urn:ietf:params:xml:ns:carddav" xmlns:cm="http://cal.me.com/_namespace/" xmlns:md="urn:mobileme:davservices">
  <D:href>/caldav/v2/gavindi%40gmail.com/user</D:href>
  <D:propstat>
   <D:status>HTTP/1.1 404 Not Found</D:status>
   <D:prop>
    <D:current-user-principal/>
   </D:prop>
  </D:propstat>
 </D:response>
</D:multistatus>"#;

    #[test]
    fn pretty_printed_multistatus_does_not_leak_whitespace_into_prop_values() {
        let home = parse_multistatus(GOOGLE_STYLE_HOME_MULTISTATUS).unwrap();
        assert_eq!(home.len(), 1);
        assert_eq!(home[0].prop(CALDAV, "calendar-home-set"), Some("/caldav/v2/gavindi%40gmail.com/"));

        let list = parse_multistatus(GOOGLE_STYLE_LIST_MULTISTATUS).unwrap();
        assert_eq!(list.len(), 2);
        let calendar = &list[0];
        assert_eq!(calendar.href, "/caldav/v2/gavindi%40gmail.com/events/");
        assert_eq!(calendar.prop(DAV, "displayname"), Some("gavindi@gmail.com"));
        assert_eq!(calendar.prop(APPLE_ICAL, "calendar-color"), Some("#9FE1E7FF"));
        // The resourcetype must survive as "collection,calendar", not a
        // whitespace blob, or the calendar-collection filter loses everything.
        assert_eq!(calendar.prop(DAV, "resourcetype"), Some("collection,calendar"));
        assert!(calendar.prop(DAV, "resourcetype").unwrap().contains("calendar"));
        // A 404 propstat's prop must still surface (Google 404s
        // current-user-principal; the empty value is what drives the
        // resolve-to-base_url fallback in discover_calendar_home).
        assert_eq!(list[1].prop(DAV, "current-user-principal"), Some(""));
    }

    #[test]
    fn propfind_and_calendar_query_bodies_are_well_formed_xml() {
        let propfind = build_propfind_body(&["D:current-user-principal"]);
        parse_multistatus(&format!("<D:multistatus xmlns:D=\"DAV:\">{propfind}</D:multistatus>")).unwrap_or_default();
        // A real well-formedness check: feed it back through the reader
        // directly and confirm no Err is produced by the underlying parser.
        let mut reader = NsReader::from_str(&propfind);
        loop {
            match reader.read_resolved_event() {
                Ok((_, Event::Eof)) => break,
                Ok(_) => continue,
                Err(e) => panic!("build_propfind_body produced malformed XML: {e}"),
            }
        }

        let start = "2026-07-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let end = "2026-08-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let query = build_calendar_query_body(start, end);
        let mut reader = NsReader::from_str(&query);
        loop {
            match reader.read_resolved_event() {
                Ok((_, Event::Eof)) => break,
                Ok(_) => continue,
                Err(e) => panic!("build_calendar_query_body produced malformed XML: {e}"),
            }
        }
        assert!(query.contains("20260701T000000Z"));
        assert!(query.contains("20260801T000000Z"));
    }

    #[test]
    fn todo_query_body_is_well_formed_xml_and_filters_on_vtodo() {
        let body = build_todo_query_body();
        assert!(body.contains("<C:todo-query"), "must use RFC 4791 §7.10 todo-query: {body}");
        assert!(body.contains("<C:comp-filter name=\"VTODO\"/>"), "must filter on VTODO: {body}");
        assert!(!body.contains("time-range"), "tasks must not be windowed: {body}");

        let mut reader = NsReader::from_str(&body);
        loop {
            match reader.read_resolved_event() {
                Ok((_, Event::Eof)) => break,
                Ok(_) => continue,
                Err(e) => panic!("build_todo_query_body produced malformed XML: {e}"),
            }
        }
    }

    #[test]
    fn addressbook_multiget_body_is_well_formed_xml_and_lists_every_href() {
        let hrefs = vec!["/carddav/v1/lists/default/card1.vcf".to_string(), "/carddav/v1/lists/default/card2.vcf".to_string()];
        let body = build_addressbook_multiget_body(&hrefs, &["D:getetag", "CD:address-data"]);
        assert!(body.contains("<D:getetag/>"));
        assert!(body.contains("<CD:address-data/>"));
        assert!(body.contains("<D:href>/carddav/v1/lists/default/card1.vcf</D:href>"));
        assert!(body.contains("<D:href>/carddav/v1/lists/default/card2.vcf</D:href>"));

        let mut reader = NsReader::from_str(&body);
        loop {
            match reader.read_resolved_event() {
                Ok((_, Event::Eof)) => break,
                Ok(_) => continue,
                Err(e) => panic!("build_addressbook_multiget_body produced malformed XML: {e}"),
            }
        }
    }

    #[test]
    fn addressbook_multiget_href_is_xml_escaped() {
        let hrefs = vec!["/lists/default/a&b.vcf".to_string()];
        let body = build_addressbook_multiget_body(&hrefs, &["D:getetag"]);
        assert!(body.contains("<D:href>/lists/default/a&amp;b.vcf</D:href>"));
    }

    #[test]
    fn sync_collection_body_is_well_formed_xml() {
        let body = build_sync_collection_body(Some("token123"), &["D:getetag", "CD:address-data"]);
        assert!(body.contains("<D:sync-token>token123</D:sync-token>"));
        assert!(body.contains("<D:getetag/>"));
        assert!(body.contains("<CD:address-data/>"));

        let mut reader = NsReader::from_str(&body);
        loop {
            match reader.read_resolved_event() {
                Ok((_, Event::Eof)) => break,
                Ok(_) => continue,
                Err(e) => panic!("build_sync_collection_body produced malformed XML: {e}"),
            }
        }
    }

    #[test]
    fn initial_sync_collection_body_still_includes_an_empty_sync_token() {
        // RFC 6578: `sync-token` must be present (empty, for "start from
        // scratch") even on the very first sync - some servers (Google's
        // CardDAV) reject the request outright if it's missing entirely.
        let body = build_sync_collection_body(None, &["D:getetag"]);
        assert!(body.contains("<D:sync-token/>"));
        assert!(
            !body.contains("<D:sync-token>\n"),
            "an empty element should be self-closed, not an open/close pair with nothing between"
        );
    }

    // RFC 6578 sync-collection response: changed members carry props (with a
    // propstat-level status), removed members are bare response-level 404s,
    // and the whole document ends with the server's next sync-token.
    const SYNC_COLLECTION_REPORT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:CD="urn:ietf:params:xml:ns:carddav">
  <D:response>
    <D:href>/carddav/v1/lists/default/changed.vcf</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"new-etag"</D:getetag>
        <CD:address-data>BEGIN:VCARD
VERSION:4.0
UID:changed
END:VCARD
</CD:address-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/carddav/v1/lists/default/removed.vcf</D:href>
    <D:status>HTTP/1.1 404 Not Found</D:status>
  </D:response>
  <D:sync-token>token-42</D:sync-token>
</D:multistatus>"#;

    #[test]
    fn sync_collection_report_surfaces_statuses_and_the_next_token() {
        let (responses, token) = parse_multistatus_with_token(SYNC_COLLECTION_REPORT).unwrap();
        assert_eq!(token.as_deref(), Some("token-42"));
        assert_eq!(responses.len(), 2);

        // The changed member: response-level status stays None (its status is
        // propstat-level, which isn't the response's), props are intact.
        assert_eq!(responses[0].href, "/carddav/v1/lists/default/changed.vcf");
        assert_eq!(responses[0].status, None);
        assert_eq!(responses[0].prop(DAV, "getetag"), Some("\"new-etag\""));
        assert!(responses[0].prop(NS_CARDDAV, "address-data").unwrap().contains("UID:changed"));

        // The removed member: response-level 404 with no props.
        assert_eq!(responses[1].href, "/carddav/v1/lists/default/removed.vcf");
        assert_eq!(responses[1].status.as_deref(), Some("HTTP/1.1 404 Not Found"));
        assert_eq!(responses[1].prop(DAV, "getetag"), None);
    }

    #[test]
    fn ordinary_multistatus_reports_no_status_or_token() {
        let (responses, token) = parse_multistatus_with_token(GOOGLE_STYLE_LIST_MULTISTATUS).unwrap();
        assert_eq!(token, None);
        assert!(
            responses.iter().all(|r| r.status.is_none()),
            "propstat-level 404s must not leak into the response-level status"
        );
    }
}
