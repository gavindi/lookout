//! Small, pure text and URL helpers shared across Lookout's crates.
//!
//! Deliberately no I/O: percent/base64url encoding, `application/x-www-form-urlencoded`
//! query parsing (used by the OAuth callback paths in `lookout-app`) and a
//! human-readable byte size (used by the rules engine's labels and the
//! attachment strips). Every helper here previously existed as a copy-pasted
//! private function in two or more app modules.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;

use base64::Engine as _;

/// RFC 3986 unreserved characters pass through; everything else becomes
/// `%XX`. Covers spaces, slashes and colons in query values and scope
/// strings (the workspace's `reqwest` doesn't enable its `.query()` feature,
/// so URLs are built by hand).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Unpadded base64url (RFC 4648 §5), as PKCE verifiers, challenges and
/// opaque OAuth values use.
pub fn base64url_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes an `application/x-www-form-urlencoded` value: `%XX` escapes and
/// `+` as space.
pub fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One hex digit's value, `None` for anything else.
pub fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parses an `application/x-www-form-urlencoded` query string into decoded
/// key/value pairs. A bare key (no `=`) maps to an empty string; a repeated
/// key keeps the last value.
pub fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let mut it = pair.splitn(2, '=');
            let k = it.next()?;
            let v = it.next().unwrap_or("");
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

/// A human-readable byte size ("2.0 MB") using binary units.
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(percent_encode("abc-_.~"), "abc-_.~");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("list/with/slash"), "list%2Fwith%2Fslash");
        assert_eq!(
            percent_encode("https://outlook.office.com/IMAP.AccessAsUser.All"),
            "https%3A%2F%2Foutlook.office.com%2FIMAP.AccessAsUser.All"
        );
        assert_eq!(percent_encode("internetMessageId eq '<a@b>'"), "internetMessageId%20eq%20%27%3Ca%40b%3E%27");
    }

    #[test]
    fn url_decode_handles_percent_escapes_and_plus() {
        assert_eq!(url_decode("a%20b+c"), "a b c");
        assert_eq!(url_decode("%2F%3A%2F"), "/:/");
        assert_eq!(url_decode("plain"), "plain");
        assert_eq!(url_decode("broken%2"), "broken%2");
    }

    #[test]
    fn parse_query_extracts_and_decodes_pairs() {
        let params = parse_query("code=abc%2B1&state=xyz&empty");
        assert_eq!(params.get("code").map(String::as_str), Some("abc+1"));
        assert_eq!(params.get("state").map(String::as_str), Some("xyz"));
        assert_eq!(params.get("empty").map(String::as_str), Some(""));
    }

    #[test]
    fn human_size_uses_binary_units_and_degrades_to_bytes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(12 * 1024 + 512), "12.5 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(human_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }
}
