//! Link safety (OUTLOOK_PARITY.md item 6.3 / S-08): warn when a message's link
//! text names a domain that differs from the domain it actually points at -
//! the classic `apple.com` → `apple.com.evil.example` bait. The reading pane
//! shows the real target on hover and confirms before opening a link flagged
//! here. Pure and I/O-free, so it is unit-tested directly.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license

/// The `href`s of anchors whose visible text is a URL/domain pointing
/// somewhere else. Only http(s) links with a parseable host are considered;
/// relative links and plain prose anchors are ignored. Order follows the
/// document, duplicates are dropped (a link repeated in a thread is one
/// warning).
pub fn suspicious_link_hrefs(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(anchor) = rest.find("<a").map(|i| &rest[i..]) {
        let Some(open_end) = anchor.find('>') else { break };
        let open_tag = &anchor[..open_end];
        let body_start = open_end + 1;
        let Some(close) = anchor[body_start..].find("</a>") else { break };
        let text = &anchor[body_start..body_start + close];
        if let Some(href) = attr_value(open_tag, "href") {
            if let (Some(text_domain), Some(href_domain)) = (url_text_domain(text), uri_host(&href)) {
                if !same_site(&text_domain, &href_domain) && !out.iter().any(|h| h == &href) {
                    out.push(href);
                }
            }
        }
        rest = &anchor[body_start + close + "</a>".len()..];
    }
    out
}

/// The value of `name` on an opening tag, matching both `"` and `'` quoting,
/// case-insensitively on the attribute name.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut from = 0;
    while let Some(pos) = lower[from..].find(&needle) {
        let start = from + pos;
        // Must be a whole attribute: preceded by whitespace or the tag name's
        // end, not part of a longer name like `data-href`.
        let preceded_ok = start == 0 || !lower.as_bytes()[start - 1].is_ascii_alphanumeric() && lower.as_bytes()[start - 1] != b'-';
        let value_start = start + needle.len();
        if !preceded_ok {
            from = value_start;
            continue;
        }
        let quote = tag.as_bytes().get(value_start).copied()?;
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        let value = &tag[value_start + 1..];
        let end = value.find(quote as char)?;
        return Some(value[..end].trim().to_string());
    }
    None
}

/// The visible text of an anchor with any nested tags stripped, whitespace
/// collapsed.
fn strip_tags(text: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The domain a hoverable text names, if it reads as a URL or a bare domain.
fn url_text_domain(text: &str) -> Option<String> {
    for token in strip_tags(text).split_whitespace() {
        let token = token.trim_matches(|c: char| "<>()[],;:\"'".contains(c));
        if let Some(host) = uri_host(token) {
            return Some(host);
        }
        if looks_like_domain(token) {
            return Some(normalize_host(token));
        }
    }
    None
}

/// The host of an `http`/`https`/`ftp` URL: userinfo, port and path stripped.
fn uri_host(uri: &str) -> Option<String> {
    let lower = uri.trim().to_ascii_lowercase();
    let after_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .or_else(|| lower.strip_prefix("ftp://"))?;
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority); // drop userinfo
    let host = host.split(':').next().unwrap_or(host); // drop port
    let host = host.trim_end_matches('.').trim();
    if host.contains('.') || host == "localhost" {
        Some(normalize_host(host))
    } else {
        None
    }
}

/// Whether a bare token reads as a domain (has a dot, no spaces, only
/// hostname characters, and a plausible alphabetic TLD).
fn looks_like_domain(token: &str) -> bool {
    let token = token.trim();
    if token.contains(char::is_whitespace) || !token.contains('.') {
        return false;
    }
    if !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        return false;
    }
    let tld = token.rsplit('.').next().unwrap_or("");
    tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_start_matches("www.").to_ascii_lowercase()
}

/// Whether two hosts are the same site: equal, or one is a subdomain of the
/// other (so `www.example.com` and `example.com` match, but
/// `example.com` and `example.com.evil.test` do not).
fn same_site(a: &str, b: &str) -> bool {
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_an_anchor_whose_text_hides_a_different_host() {
        let html = r#"<p>Click <a href="https://apple.com.evil.test/login">apple.com</a> now</p>"#;
        assert_eq!(suspicious_link_hrefs(html), vec!["https://apple.com.evil.test/login"]);
    }

    #[test]
    fn accepts_matching_hosts_and_subdomains() {
        let html = r#"<a href="https://www.example.com/x">example.com</a>
            <a href="https://mail.example.com">mail.example.com</a>
            <a href="https://example.com">www.example.com</a>"#;
        assert!(suspicious_link_hrefs(html).is_empty());
    }

    #[test]
    fn ignores_plain_prose_and_relative_links() {
        let html = r#"<a href="/local/path">Read more</a> <a href="mailto:a@b.com">A B</a> <a href="https://example.com">Home</a>"#;
        assert!(suspicious_link_hrefs(html).is_empty());
    }

    #[test]
    fn reads_single_quoted_hrefs_and_strips_nested_tags() {
        let html = r#"<a href='http://paypal.com.secure-login.test/'><b>paypal.com</b></a>"#;
        assert_eq!(suspicious_link_hrefs(html), vec!["http://paypal.com.secure-login.test/"]);
    }

    #[test]
    fn deduplicates_repeated_links() {
        let html = r#"<a href="https://evil.test">bank.com</a><a href="https://evil.test">bank.com</a>"#;
        assert_eq!(suspicious_link_hrefs(html), vec!["https://evil.test"]);
    }
}
