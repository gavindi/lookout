//! `@mentions` detection (OUTLOOK_PARITY.md §5.1, M-25 / item 2.13).
//!
//! Outlook/Exchange computes "mentions me" server-side and exposes it over
//! IMAP as nothing at all, so a message body has to be inspected client-side
//! to decide whether it mentions the user. Two signals are recognised, both
//! pure string scanning (no HTML parser - matching how `trust` and
//! `link_safety` scan markup):
//!
//! - **An address token**: the user's own address appearing as a standalone
//!   token. This covers an authored `mailto:` link (`mailto:me@example.com`),
//!   a linkified bare address, and a plain address in quoted text, without
//!   needing to know which form it took.
//! - **An `@Name` mention**: an `@` at the start of a word followed by a token
//!   equal to one of the user's names (a display name, or an address
//!   local-part). The `@` must not sit inside an email address
//!   (`foo@example.com` is not a mention of `@example`).
//!
//! Everything is case-insensitive and boundary-checked, so `ome@example.com`
//! is not a mention of `me@example.com`.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashSet;

/// The user's identities, compiled once and matched against a body snippet.
///
/// Addresses are compared against standalone address tokens; names against an
/// `@Name` token. Both are normalised (trimmed, ASCII-lowercased, deduped and
/// empties dropped) at construction, so a body can be scanned repeatedly
/// without re-normalising.
#[derive(Debug, Clone, Default)]
pub struct MentionMatcher {
    addresses: Vec<String>,
    names: HashSet<String>,
}

impl MentionMatcher {
    /// Builds a matcher from the user's addresses and names. `names` should
    /// include any form a correspondent might write after an `@` - a full
    /// display name, its first word, and address local-parts; the session
    /// supplies them (see `AccountConfig`'s mention fields).
    pub fn new(addresses: impl IntoIterator<Item = impl AsRef<str>>, names: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut seen = HashSet::new();
        let addresses = addresses
            .into_iter()
            .map(|a| a.as_ref().trim().to_ascii_lowercase())
            .filter(|a| !a.is_empty() && seen.insert(a.clone()))
            .collect();
        let names = names.into_iter().map(|n| n.as_ref().trim().to_ascii_lowercase()).filter(|n| !n.is_empty()).collect();
        MentionMatcher { addresses, names }
    }

    /// Whether the matcher has anything to look for. An empty matcher never
    /// matches, so a caller with no known identities degrades to "not
    /// mentioned" rather than flagging every message.
    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty() && self.names.is_empty()
    }

    /// Whether `haystack` (plain body text or message HTML) mentions the user.
    pub fn matches(&self, haystack: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        // ASCII-only lowercasing preserves byte length, so the byte offsets
        // the scanners below use stay valid against the original string.
        let lower = haystack.to_ascii_lowercase();
        self.addresses.iter().any(|address| contains_address_token(&lower, address)) || contains_name_mention(&lower, &self.names)
    }
}

/// Whether `token` occurs in `haystack` as a standalone token - flanked by
/// bytes that can't be part of an address on either side.
fn contains_address_token(haystack: &str, token: &str) -> bool {
    haystack.match_indices(token).any(|(start, _)| {
        let before = start == 0 || !is_address_local_byte(haystack.as_bytes()[start - 1]);
        let end = start + token.len();
        let after = end >= haystack.len() || !is_address_local_byte(haystack.as_bytes()[end]);
        before && after
    })
}

/// Whether an `@Name` token matching one of `names` occurs in `haystack`.
fn contains_name_mention(haystack: &str, names: &HashSet<String>) -> bool {
    let bytes = haystack.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        if byte != b'@' {
            continue;
        }
        // Skip an `@` embedded in an email address (`foo@example.com`).
        if index > 0 && bytes[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && is_name_byte(bytes[end]) {
            end += 1;
        }
        // Trim trailing sentence punctuation so `@Gavin.` at the end of a
        // clause still reads as `Gavin`.
        while end > start && matches!(bytes[end - 1], b'.' | b'-' | b'_') {
            end -= 1;
        }
        if end > start && names.contains(&haystack[start..end]) {
            return true;
        }
    }
    false
}

/// Bytes that can extend an address local-part (used on both flanks of an
/// address match, the stricter of the two sides).
fn is_address_local_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
}

/// Bytes that can form an `@Name` token.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'\'')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher() -> MentionMatcher {
        MentionMatcher::new(["me@example.com", "Other@Example.com"], ["Gavin", "Gavin Graham", "me"])
    }

    #[test]
    fn matches_an_address_in_a_mailto_anchor() {
        assert!(matcher().matches(r#"See <a href="mailto:me@example.com">this</a>."#));
    }

    #[test]
    fn matches_a_bare_address_token() {
        assert!(matcher().matches("Ping me@example.com when you're free."));
        assert!(matcher().matches("Copied <me@example.com> and others."));
    }

    #[test]
    fn address_matching_is_case_insensitive() {
        assert!(matcher().matches("Mentioned ME@EXAMPLE.COM"));
        assert!(matcher().matches("other@example.com"));
    }

    #[test]
    fn an_address_inside_a_larger_token_does_not_match() {
        assert!(!matcher().matches("ome@example.com is not me"));
        assert!(!matcher().matches("me@example.com.evil.example"));
        assert!(!matcher().matches("xme@example.com"));
    }

    #[test]
    fn matches_at_name_forms() {
        assert!(matcher().matches("Thanks @Gavin for the review"));
        assert!(matcher().matches("@Gavin Graham please confirm"));
        assert!(matcher().matches("cc @me on the reply"));
    }

    #[test]
    fn an_at_inside_an_email_is_not_a_name_mention() {
        // `@example` would match nothing here, but prove the guard with a
        // matcher whose name collides with a domain label.
        let m = MentionMatcher::new(Vec::<String>::new(), ["example"]);
        assert!(!m.matches("bounce@example.com"));
        assert!(m.matches("@example is a label"));
    }

    #[test]
    fn empty_matcher_matches_nothing() {
        let m = MentionMatcher::new(Vec::<String>::new(), Vec::<String>::new());
        assert!(m.is_empty());
        assert!(!m.matches("@anyone me@example.com"));
    }

    #[test]
    fn name_mention_stops_at_punctuation() {
        let m = MentionMatcher::new(Vec::<String>::new(), ["gavin"]);
        assert!(m.matches("@gavin, can you look?"));
        assert!(m.matches("@gavin."));
        assert!(!m.matches("@gavin's team")); // token is "gavin's", not "gavin"
    }
}
