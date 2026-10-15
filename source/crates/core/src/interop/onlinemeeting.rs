//! Online-meeting URL detection (OUTLOOK_PARITY.md §4.3, CAL-20 / 3.17).
//!
//! Outlook stamps an online meeting's join URL in several places and joins
//! everywhere else in the trade: the RFC 7986 `CONFERENCE` property, its own
//! `X-MICROSOFT-SKYPETEAMSMEETINGURL` / `X-MICROSOFT-ONLINEMEETINGCONFLINK`
//! VEVENT properties, and - most reliably - inside the `DESCRIPTION` text
//! ("Microsoft Teams meeting\nJoin: https://teams.microsoft.com/…"). This
//! module turns all of those into one join URL plus the provider it belongs
//! to, so the invite card, reminder and event peek can show a **Join** button
//! without any feature code hard-coding a vendor's URL shape (see §2.4
//! decision 3: the mapping lives here, once).

/// The Microsoft VEVENT property Outlook uses for a Teams meeting URL.
pub const X_MICROSOFT_SKYPETEAMSMEETINGURL: &str = "X-MICROSOFT-SKYPETEAMSMEETINGURL";
/// The companion Microsoft VEVENT property carrying the same join URL.
pub const X_MICROSOFT_ONLINEMEETINGCONFLINK: &str = "X-MICROSOFT-ONLINEMEETINGCONFLINK";

/// The service an online-meeting URL belongs to. `Other` is used for an
/// explicit join URL on an unrecognised host (a self-hosted or niche
/// provider) - callers can still show a plain "Join".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnlineMeetingProvider {
    Teams,
    Zoom,
    GoogleMeet,
    Webex,
    Jitsi,
    Other,
}

impl OnlineMeetingProvider {
    /// The provider name for a "Join …" affordance.
    pub fn display_name(self) -> &'static str {
        match self {
            OnlineMeetingProvider::Teams => "Teams",
            OnlineMeetingProvider::Zoom => "Zoom",
            OnlineMeetingProvider::GoogleMeet => "Google Meet",
            OnlineMeetingProvider::Webex => "Webex",
            OnlineMeetingProvider::Jitsi => "Jitsi",
            OnlineMeetingProvider::Other => "online meeting",
        }
    }
}

/// A detected online meeting: the join URL and the provider it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlineMeeting {
    pub url: String,
    pub provider: OnlineMeetingProvider,
}

/// Classifies `url` by its host, returning `None` for a URL that isn't a
/// recognised online-meeting provider. Compared case-insensitively and
/// host-only (the path - `/l/meetup-join/…`, `/j/…`, … - is not needed).
pub fn classify(url: &str) -> Option<OnlineMeetingProvider> {
    let host = host_of(url)?;
    let matches = |base: &str| host == base || host.ends_with(&format!(".{base}"));
    if matches("teams.microsoft.com") || matches("teams.live.com") {
        return Some(OnlineMeetingProvider::Teams);
    }
    if matches("zoom.us") || matches("zoom.com") {
        return Some(OnlineMeetingProvider::Zoom);
    }
    if matches("meet.google.com") {
        return Some(OnlineMeetingProvider::GoogleMeet);
    }
    if matches("webex.com") {
        return Some(OnlineMeetingProvider::Webex);
    }
    if matches("meet.jit.si") || matches("jit.si") {
        return Some(OnlineMeetingProvider::Jitsi);
    }
    None
}

/// Whether a join URL is a Microsoft Teams meeting (`classify` == Teams).
/// The model's `CalendarEvent::is_online_meeting` flag isn't carried on
/// `EventOccurrence`, so reconstructing an event from an occurrence uses this
/// on its `conference_url` to preserve the Teams identity on a Graph write.
pub fn is_teams_meeting(url: Option<&str>) -> bool {
    url.and_then(classify) == Some(OnlineMeetingProvider::Teams)
}

/// Picks the online meeting an event offers. An explicit join URL
/// (`CONFERENCE`, the `X-MICROSOFT-*` properties, or `URL`) is trusted first
/// and always wins - an unrecognised host becomes [`OnlineMeetingProvider::Other`],
/// so a self-hosted link keeps working. When there is none, the `location` and
/// then the `description` text are scanned for a URL that [`classify`]s as a
/// known provider (this is how the Teams link Outlook buries in the
/// description is found). Returns `None` when nothing is a recognised meeting.
pub fn detect(explicit_url: Option<&str>, location: Option<&str>, description: Option<&str>) -> Option<OnlineMeeting> {
    if let Some(url) = explicit_url.map(str::trim).filter(|u| !u.is_empty()) {
        return Some(OnlineMeeting {
            url: url.to_string(),
            provider: classify(url).unwrap_or(OnlineMeetingProvider::Other),
        });
    }
    for text in [location, description].into_iter().flatten() {
        for url in urls_in(text) {
            if let Some(provider) = classify(&url) {
                return Some(OnlineMeeting { url, provider });
            }
        }
    }
    None
}

/// The lowercase host of an absolute `http(s)` URL, or `None` for anything
/// else (non-http schemes, relative strings, a bare word). No `url` crate in
/// this workspace, so this is a deliberately small parse: strip the scheme,
/// drop any `userinfo@`, and take everything up to the first `/`, `?` or `#`.
fn host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    // Trim a `:port` (the last colon, unless it's inside an IPv6 literal).
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Every `http(s)` URL token in `text`, in order. A token runs until the next
/// character that can't legitimately end a URL in prose (whitespace, a quote,
/// an angle bracket) followed by common trailing punctuation.
fn urls_in(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = text;
    while let Some(start) = find_scheme(rest) {
        let candidate = &rest[start..];
        let end = candidate
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '\\'))
            .unwrap_or(candidate.len());
        let url = candidate[..end].trim_end_matches(['.', ',', ';', ')', ']']).to_string();
        if !url.is_empty() {
            urls.push(url);
        }
        rest = &candidate[end..];
    }
    urls
}

/// The byte offset of the next `http://` or `https://` in `text`.
fn find_scheme(text: &str) -> Option<usize> {
    text.find("https://").or_else(|| text.find("http://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_each_known_provider_by_host() {
        assert_eq!(classify("https://teams.microsoft.com/l/meetup-join/abc"), Some(OnlineMeetingProvider::Teams));
        assert_eq!(classify("https://teams.live.com/meet/123"), Some(OnlineMeetingProvider::Teams));
        assert_eq!(classify("https://us02web.zoom.us/j/12345"), Some(OnlineMeetingProvider::Zoom));
        assert_eq!(classify("https://zoom.us/j/1"), Some(OnlineMeetingProvider::Zoom));
        assert_eq!(classify("https://meet.google.com/abc-defg-hij"), Some(OnlineMeetingProvider::GoogleMeet));
        assert_eq!(classify("https://contoso.webex.com/meet/bob"), Some(OnlineMeetingProvider::Webex));
        assert_eq!(classify("https://meet.jit.si/SomeRoom"), Some(OnlineMeetingProvider::Jitsi));
    }

    #[test]
    fn classification_ignores_non_meeting_and_malformed_urls() {
        assert_eq!(classify("https://example.com/join"), None);
        assert_eq!(classify("mailto:bob@example.com"), None);
        assert_eq!(classify("not a url"), None);
        assert_eq!(classify(""), None);
        // A lookalike host must not match a suffix that isn't a real domain
        // boundary.
        assert_eq!(classify("https://notteams.microsoft.com.evil.example/"), None);
    }

    #[test]
    fn explicit_url_wins_even_on_an_unknown_host() {
        let meeting = detect(Some("https://meet.example.com/room/1"), None, None).expect("explicit URL is trusted");
        assert_eq!(meeting.url, "https://meet.example.com/room/1");
        assert_eq!(meeting.provider, OnlineMeetingProvider::Other);
    }

    #[test]
    fn detects_the_teams_url_outlook_buries_in_the_description() {
        let description = "Microsoft Teams meeting\nJoin: https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0\nMeeting ID: 123";
        let meeting = detect(None, Some("Microsoft Teams Meeting"), Some(description)).expect("description URL found");
        assert_eq!(meeting.provider, OnlineMeetingProvider::Teams);
        assert_eq!(meeting.url, "https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0");
    }

    #[test]
    fn scanning_skips_non_meeting_links_until_a_known_one() {
        let description = "Agenda: https://example.com/agenda and dial-in https://zoom.us/j/42.";
        let meeting = detect(None, None, Some(description)).expect("zoom link found");
        assert_eq!(meeting.provider, OnlineMeetingProvider::Zoom);
        // Trailing sentence punctuation is not part of the URL.
        assert_eq!(meeting.url, "https://zoom.us/j/42");
        // A description with no meeting link at all yields nothing.
        assert_eq!(detect(None, None, Some("See https://example.com/")), None);
        assert_eq!(detect(None, None, None), None);
    }
}
