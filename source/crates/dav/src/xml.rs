//! Minimal WebDAV/CalDAV `multistatus` XML support: request-body builders
//! for the handful of PROPFIND/REPORT queries this crate needs, and a
//! namespace-aware parser for the responses.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::reader::NsReader;

use crate::error::Result;

/// One `<response>` entry from a `multistatus` document: its href, plus
/// whatever requested properties it reported, keyed by `(namespace, local
/// name)` - resolved via the server's actual namespace URIs, not raw
/// (possibly server-specific) prefix strings, so a server using `xmlns:x1=`
/// instead of the conventional `xmlns:d=` still parses correctly.
#[derive(Debug, Clone, Default)]
pub struct DavResponse {
    pub href: String,
    props: HashMap<(Vec<u8>, String), String>,
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
}

fn ns_bytes(ns: &ResolveResult) -> Vec<u8> {
    match ns {
        ResolveResult::Bound(Namespace(b)) => b.to_vec(),
        _ => Vec::new(),
    }
}

#[derive(Default)]
struct ParseState {
    responses: Vec<DavResponse>,
    current: Option<DavResponse>,
    response_depth: Option<u32>,
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
}

impl ParseState {
    fn on_open(&mut self, depth: u32, ns: &ResolveResult, local: &str) {
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
        if let Some(pd) = self.prop_depth {
            if depth == pd + 1 {
                self.prop_key = Some((ns_bytes(ns), local.to_string()));
                self.prop_text.clear();
                self.prop_child_names.clear();
            } else if depth > pd + 1 {
                self.prop_child_names.push(local.to_string());
            }
        }
    }

    fn on_text(&mut self, depth: u32, text: &str) {
        if self.href_open_depth == Some(depth) {
            self.href_text.push_str(text);
        } else if let Some(pd) = self.prop_depth {
            if depth == pd + 1 && self.prop_key.is_some() {
                self.prop_text.push_str(text);
            }
        }
    }

    fn on_close(&mut self, depth: u32) {
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
                    let value = if !self.prop_child_names.is_empty() && self.prop_text.is_empty() {
                        self.prop_child_names.join(",")
                    } else {
                        self.prop_text.clone()
                    };
                    resp.props.insert(key, value);
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
                state.on_open(depth, &ns, &local);
            }
            Event::Empty(e) => {
                depth += 1;
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                state.on_open(depth, &ns, &local);
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

/// Builds a `PROPFIND` request body asking for the given (already-prefixed)
/// property names, e.g. `&["D:current-user-principal"]` or
/// `&["D:displayname", "D:resourcetype", "IC:calendar-color"]`.
pub fn build_propfind_body(props: &[&str]) -> String {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n\
         <D:propfind xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:IC=\"http://apple.com/ns/ical/\">\n  <D:prop>\n",
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
}
