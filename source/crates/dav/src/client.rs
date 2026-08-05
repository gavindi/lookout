use chrono::{DateTime, Utc};
use lookout_core::{AccountId, CalendarEvent, CalendarId, CalendarInfo, VCard};
use reqwest::Method;

use crate::config::Credential;
use crate::error::{Error, Result};
use crate::ical;
use crate::xml::{self, DavResponse, NS_CARDDAV};

const NS_DAV: &[u8] = b"DAV:";
const NS_CALDAV: &[u8] = b"urn:ietf:params:xml:ns:caldav";
const NS_APPLE_ICAL: &[u8] = b"http://apple.com/ns/ical/";

const ADDRESSBOOK_PROPS: [&str; 2] = ["D:displayname", "D:resourcetype"];

#[derive(Debug, Clone)]
pub struct AddressBookInfo {
    pub account_id: AccountId,
    pub display_name: String,
    pub href: String,
}

impl AddressBookInfo {
    fn new(account_id: &AccountId, href: &str, display_name: String) -> Self {
        Self { account_id: account_id.clone(), display_name, href: href.to_string() }
    }
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
        let responses = self.fetch_addressbook_contacts(addressbook, credential).await?;
        let mut vcards = Vec::new();
        for response in responses {
            let Some(data) = response.prop(NS_CARDDAV, "address-data") else { continue };
            // One malformed/unsupported card (a version this parser doesn't
            // handle, a server-specific quirk, ...) used to abort the whole
            // batch via `?` - discarding every other card the response also
            // contained. Skip just that one instead; the rest of the account
            // shouldn't go blank over a single bad card.
            match VCard::parse(data) {
                Ok(card) => vcards.push(card),
                Err(e) => tracing::warn!("skipping unparseable vCard at {:?}: {e}", response.href),
            }
        }
        Ok(vcards)
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
        let base_url = reqwest::Url::parse(base_url).map_err(|e| Error::Discovery(format!("invalid base URL {base_url:?}: {e}")))?;
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
    pub async fn list_calendars(&self, home_set_href: &str, account_id: &AccountId, credential: &Credential) -> Result<Vec<CalendarInfo>> {
        let home_url = self.resolve(home_set_href)?;
        let responses = self.propfind(home_url, 1, credential, &["D:displayname", "D:resourcetype", "IC:calendar-color"]).await?;

        Ok(responses
            .into_iter()
            .filter(|r| r.prop(NS_DAV, "resourcetype").unwrap_or("").contains("calendar"))
            .map(|r| CalendarInfo {
                id: CalendarId::new(account_id, &r.href),
                account_id: account_id.clone(),
                display_name: r.prop(NS_DAV, "displayname").unwrap_or("").to_string(),
                color: r.prop(NS_APPLE_ICAL, "calendar-color").map(str::to_string),
                href: r.href.clone(),
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
            .filter_map(|r| r.prop(NS_CALDAV, "calendar-data").map(str::to_string))
            .flat_map(|ics| ical::parse_vevents(&calendar.id, &ics))
            .collect())
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

    async fn send_xml_request(&self, method_name: &str, url: reqwest::Url, depth: u8, credential: &Credential, body: String) -> Result<Vec<DavResponse>> {
        let method = Method::from_bytes(method_name.as_bytes()).expect("method name is a valid HTTP token");
        let request_url = url.clone();
        tracing::debug!("DAV {method_name} {request_url} (Depth: {depth}) body:\n{body}");
        let mut req = self
            .http
            .request(method, url)
            .header("Depth", depth.to_string())
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body);
        req = match credential {
            // CalDAV's actual auth mechanism is plain HTTP headers, not the
            // SASL-inside-IMAP `AUTHENTICATE XOAUTH2` Mail uses - nothing to
            // share between the two crates here.
            Credential::Password(password) => req.basic_auth(&self.username, Some(password)),
            Credential::OAuth2AccessToken(token) => req.bearer_auth(token),
        };

        let response = req.send().await?;
        let status = response.status();
        let text = response.text().await?;
        tracing::debug!("DAV {method_name} {request_url} -> {status}, body:\n{text}");
        if !status.is_success() {
            // `error_for_status()` would discard the response body here -
            // and CardDAV/CalDAV servers (Google, Nextcloud, ...) routinely
            // explain exactly what they didn't like about the request in it,
            // which is far more useful for diagnosing a 4xx than a bare
            // "400 Bad Request" with no context.
            let snippet: String = text.chars().take(500).collect();
            return Err(Error::Discovery(format!("HTTP {status} for {request_url}: {snippet}")));
        }
        xml::parse_multistatus(&text)
    }
}

#[cfg(test)]
mod tests {
    use lookout_core::AccountId;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

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
        assert_eq!(calendars.len(), 1, "the home collection itself must not be mistaken for a calendar");
        assert_eq!(calendars[0].display_name, "Personal");
        assert_eq!(calendars[0].href, "/calendars/alice/home/personal/");

        let start = "2026-07-01T00:00:00Z".parse().unwrap();
        let end = "2026-08-01T00:00:00Z".parse().unwrap();
        let events = client.fetch_events_in_range(&calendars[0], start, end, &credential).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid.0, "evt-1@example.com");
        assert_eq!(events[0].summary.as_deref(), Some("Team sync"));
    }
}
