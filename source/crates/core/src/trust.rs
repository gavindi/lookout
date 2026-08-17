//! Sender-trust policy for the reading pane's remote-content blocking:
//! per-sender entries (an exact address or an `@domain`) carrying a trust
//! level, plus the HTML scan that decides whether a rendered message
//! references remote content at all.
//!
//! Everything here is pure string logic: WebKit's `decide-policy` handler
//! stays the authoritative blocker of remote subresources, and this module
//! only decides which stored entries match a sender and whether the
//! reading pane's external-content banner is worth showing.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
/// How much remote content a trusted sender may load.
///
/// `Images` is the conservative default - it relaxes exactly the same
/// `image/*` predicate Config → Mail's "Load images from the web" toggle
/// relaxes. `AllContent` additionally passes stylesheets, fonts and media;
/// the reading pane's link-click/iframe vetoes and disabled JavaScript
/// still apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum TrustLevel {
    /// Remote `image/*` subresources only.
    #[default]
    Images,
    /// Every remote subresource response (images, stylesheets, fonts, media).
    AllContent,
}

impl TrustLevel {
    /// The on-disk encoding used by the UI-state database. Unknown values
    /// (a schema/format change mid-flight) resolve to the conservative
    /// `Images` rather than failing the load.
    pub fn from_i64(value: i64) -> Self {
        match value {
            2 => TrustLevel::AllContent,
            _ => TrustLevel::Images,
        }
    }

    pub fn as_i64(self) -> i64 {
        match self {
            TrustLevel::Images => 1,
            TrustLevel::AllContent => 2,
        }
    }

    /// A short display label for the manage dialog's level badges.
    pub fn label(self) -> &'static str {
        match self {
            TrustLevel::Images => "Images only",
            TrustLevel::AllContent => "All content",
        }
    }
}

/// Normalizes a user-typed trusted-sender entry: `name@example.com` (an
/// exact address) or `@example.com` (every address on that domain),
/// trimmed and lowercased. Returns `None` when the input isn't a plausible
/// entry, so the manage dialog can reject it rather than persist junk.
pub fn normalize_trust_entry(input: &str) -> Option<String> {
    let entry = input.trim().to_lowercase();
    if let Some(domain) = entry.strip_prefix('@') {
        if !domain.is_empty() && domain.contains('.') && !domain.contains('@') {
            return Some(format!("@{domain}"));
        }
        return None;
    }
    let mut parts = entry.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) if !local.is_empty() && domain.contains('.') => Some(entry),
        _ => None,
    }
}

/// Whether a message's sender address matches a stored trust entry:
/// exact equality for `name@example.com`, or a `@example.com` domain
/// suffix. Entries are stored normalized (lowercase) and the sender is
/// lowercased by the caller; this also defends against a stray
/// capitalization difference anyway.
pub fn sender_matches_trust_entry(address: &str, entry: &str) -> bool {
    let address = address.trim().to_lowercase();
    if let Some(domain) = entry.strip_prefix('@') {
        address.ends_with(&format!("@{domain}"))
    } else {
        address == entry
    }
}

/// The result of scanning a message's HTML for remote subresource
/// references: whether it pulls in remote images and/or other remote
/// content (stylesheets, fonts, media). Only advisory - WebKit's
/// `decide-policy` handler is the authoritative blocker - but it decides
/// whether the reading pane's external-content banner is worth showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemoteContentScan {
    pub has_images: bool,
    pub has_other: bool,
}

impl RemoteContentScan {
    pub fn any(self) -> bool {
        self.has_images || self.has_other
    }
}

/// Whether a URL path (query/fragment stripped) looks like an image
/// WebKit will hand the policy handler with an `image/*` mime type.
fn is_image_url(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or(path).to_lowercase();
    ["png", "jpg", "jpeg", "gif", "webp", "avif", "svg", "bmp", "ico", "tiff", "tif", "jxl", "heic"]
        .iter()
        .any(|ext| path.ends_with(&format!(".{ext}")))
}

/// The next `http(s)://` occurrence at or after `from`, or `None`.
fn find_http_url(haystack: &str, from: usize) -> Option<usize> {
    let rest = &haystack[from..];
    let mut search_from = 0;
    while let Some(rel) = rest[search_from..].find("http") {
        let at = search_from + rel;
        if rest[at..].starts_with("https://") || rest[at..].starts_with("http://") {
            return Some(from + at);
        }
        search_from = at + 4;
    }
    None
}

/// Scans message HTML (mail-parser's decoded `text/html` part) for remote
/// `http(s)://` subresource references - the same loads WebKit's response
/// veto blocks. `cid:`/`data:` references (inline parts, the body itself)
/// and relative URLs (which can't resolve without a base URI anyway) never
/// start with `http(s)` and are skipped by construction.
///
/// A URL counts as a subresource reference only when a subresource marker
/// (`src=`, `srcset=`, CSS `url(`, `@import`, `poster=`, or a `<link …>`
/// `href=`) precedes it; a plain `<a href=…>` link is a navigation, not a
/// blocked load. The image/other split is an extension guess - the
/// authoritative split is the mime type WebKit sees.
pub fn html_remote_content_scan(html: &str) -> RemoteContentScan {
    let mut scan = RemoteContentScan::default();
    let bytes = html.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = find_http_url(html, from) {
        // Walk back across the preceding characters (bounded - attributes
        // and CSS declarations are short) to the last structural break
        // (`>`, `{`, `;`), collecting the context the URL belongs to.
        let mut context_start = rel;
        while context_start > 0 && rel - context_start <= 48 {
            if matches!(bytes[context_start - 1], b'>' | b'{' | b';') {
                break;
            }
            context_start -= 1;
        }
        let tight: String = html[context_start..rel].chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
        let has_href = tight.contains("href=");
        let is_link_tag = tight.contains("<link");
        let is_ref = ["src=", "srcset=", "url(", "@import", "poster="].iter().any(|m| tight.contains(m)) || (has_href && is_link_tag);
        // The URL token: from the scheme onward until whitespace or a
        // quote/`>`/`)` boundary.
        let mut url_end = rel;
        while url_end < html.len() {
            let c = bytes[url_end];
            if c.is_ascii_whitespace() || matches!(c, b'"' | b'\'' | b')' | b'>' | b'{' | b'}' | b';' | b'[' | b']') {
                break;
            }
            url_end += 1;
        }
        if is_ref {
            let path = &html[rel..url_end];
            if is_image_url(path) {
                scan.has_images = true;
            } else {
                scan.has_other = true;
            }
        }
        // Advance past this URL (and the "http" text) so overlapping
        // references are still found; the end of the loop is the end of
        // the input.
        from = url_end.max(rel + 4);
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_level_encodes_stably() {
        assert_eq!(TrustLevel::Images.as_i64(), 1);
        assert_eq!(TrustLevel::AllContent.as_i64(), 2);
        assert_eq!(TrustLevel::from_i64(1), TrustLevel::Images);
        assert_eq!(TrustLevel::from_i64(2), TrustLevel::AllContent);
        // Unknown encodings fall back to the conservative level, so a
        // schema/format change can never silently upgrade trust.
        assert_eq!(TrustLevel::from_i64(0), TrustLevel::Images);
        assert_eq!(TrustLevel::from_i64(99), TrustLevel::Images);
        assert_eq!(TrustLevel::Images.label(), "Images only");
        assert_eq!(TrustLevel::AllContent.label(), "All content");
    }

    #[test]
    fn normalize_trust_entry_accepts_addresses_and_domains() {
        assert_eq!(normalize_trust_entry("  Ada@Example.COM ").as_deref(), Some("ada@example.com"));
        assert_eq!(normalize_trust_entry("news@example.com").as_deref(), Some("news@example.com"));
        assert_eq!(normalize_trust_entry("@Example.COM").as_deref(), Some("@example.com"));
        // Junk is rejected rather than persisted.
        assert_eq!(normalize_trust_entry(""), None);
        assert_eq!(normalize_trust_entry("not-an-address"), None);
        assert_eq!(normalize_trust_entry("a@b"), None);
        assert_eq!(normalize_trust_entry("a@b@c.com"), None);
        assert_eq!(normalize_trust_entry("@"), None);
        assert_eq!(normalize_trust_entry("@nodots"), None);
    }

    #[test]
    fn sender_matches_exact_address_and_domain_entries() {
        assert!(sender_matches_trust_entry("ada@example.com", "ada@example.com"));
        assert!(sender_matches_trust_entry("ADA@Example.COM", "ada@example.com"));
        assert!(sender_matches_trust_entry("ada@example.com", "@example.com"));
        assert!(sender_matches_trust_entry("grace.hopper@example.com", "@example.com"));
        // A domain entry never matches other domains or a bare domain
        // spelled as an address.
        assert!(!sender_matches_trust_entry("ada@other.org", "@example.com"));
        assert!(!sender_matches_trust_entry("ada@example.com", "@other.org"));
        assert!(!sender_matches_trust_entry("ada@example.com", "example.com"));
        assert!(!sender_matches_trust_entry("ada@example.com", "@example.org"));
        // An exact entry never matches a different sender on the same domain.
        assert!(!sender_matches_trust_entry("grace@example.com", "ada@example.com"));
    }

    #[test]
    fn scan_ignores_local_and_relative_references() {
        let scan = html_remote_content_scan("<img src=\"cid:logo123@host\"><img src=\"data:image/png;base64,AAAA\"><img src=\"logo.png\"><a href=\"mailto:x@y.z\">m</a>");
        assert!(!scan.any());
        assert!(!html_remote_content_scan("plain text, no tags").any());
        assert!(!html_remote_content_scan("").any());
    }

    #[test]
    fn scan_finds_remote_images() {
        let scan = html_remote_content_scan(r#"<img src="https://track.example/px.gif"><img src = "https://c.example/a.webp?size=2x">"#);
        assert!(scan.has_images);
        assert!(!scan.has_other);
        // srcset candidates count too.
        let scan = html_remote_content_scan(r#"<img srcset="https://a.example/a.png 1x, https://b.example/b.jpg 2x">"#);
        assert!(scan.has_images);
        assert!(!scan.has_other);
        // CSS url() image references.
        let scan = html_remote_content_scan("<style>body { background: url(https://d.example/bg.jpg); }</style>");
        assert!(scan.has_images);
        assert!(!scan.has_other);
    }

    #[test]
    fn scan_finds_other_remote_content() {
        let scan = html_remote_content_scan(r#"<link rel="stylesheet" href="https://c.example/style.css">"#);
        assert!(scan.has_other);
        assert!(!scan.has_images);
        let scan = html_remote_content_scan(r#"@import "https://e.example/font.css";"#);
        assert!(scan.has_other);
        let scan = html_remote_content_scan(r#"<video src="https://v.example/clip.mp4"></video>"#);
        assert!(scan.has_other);
        let scan = html_remote_content_scan(r#"<script src="https://s.example/app.js"></script>"#);
        assert!(scan.has_other);
        // A plain link is a navigation, not a blocked subresource.
        let scan = html_remote_content_scan(r#"<a href="https://example.com/page">click me</a>"#);
        assert!(!scan.any());
    }

    #[test]
    fn scan_reports_both_kinds_together() {
        let scan = html_remote_content_scan(r#"<img src="https://a.example/px.gif"><link rel="stylesheet" href="https://c.example/style.css">"#);
        assert!(scan.has_images);
        assert!(scan.has_other);
        assert!(scan.any());
    }
}
