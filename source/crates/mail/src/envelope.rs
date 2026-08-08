use std::collections::BTreeSet;

use async_imap::types::Fetch;
use chrono::{DateTime, Utc};
use lookout_core::{EmailAddress, EmailSummary, MailboxId, SystemFlagBit, ThreadKey, Uid};

/// Decodes RFC 2047 MIME encoded-words (`=?charset?Q|B?...?=`) found in a
/// raw header value. IMAP's `ENVELOPE` response returns header fields
/// verbatim, still encoded, so subjects/names need this before display.
/// Supports UTF-8, US-ASCII, and ISO-8859-1/Latin-1 (the overwhelming
/// majority of real-world mail); any other declared charset falls back to a
/// lossy UTF-8 decode of the raw bytes rather than failing outright.
pub fn decode_mime_words(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("=?") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(q1) = after.find('?') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let charset = &after[..q1];
        let after_charset = &after[q1 + 1..];
        let Some(q2) = after_charset.find('?') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let encoding = &after_charset[..q2];
        let after_encoding = &after_charset[q2 + 1..];
        let Some(end) = after_encoding.find("?=") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let encoded_text = &after_encoding[..end];

        let bytes: Option<Vec<u8>> = match encoding.to_ascii_uppercase().as_str() {
            "B" => base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded_text).ok(),
            "Q" => Some(decode_q_encoding(encoded_text)),
            _ => None,
        };

        match bytes {
            Some(bytes) => out.push_str(&decode_charset(&bytes, charset)),
            None => out.push_str(&rest[start..start + 2 + q1 + 1 + q2 + 1 + end + 2]),
        }

        rest = &after_encoding[end + 2..];
        // Whitespace directly between two encoded-words is part of the
        // folding syntax, not literal content (RFC 2047 §2).
        if rest.starts_with(' ') && rest.trim_start().starts_with("=?") {
            rest = rest.trim_start();
        }
    }
    out.push_str(rest);
    out
}

fn decode_q_encoding(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        match b {
            b'_' => out.push(b' '),
            b'=' => {
                let hi = chars.next();
                let lo = chars.next();
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    if let (Some(hi), Some(lo)) = ((hi as char).to_digit(16), (lo as char).to_digit(16)) {
                        out.push(((hi << 4) | lo) as u8);
                        continue;
                    }
                }
            }
            _ => out.push(b),
        }
    }
    out
}

fn decode_charset(bytes: &[u8], charset: &str) -> String {
    match charset.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" | "us-ascii" | "ascii" => String::from_utf8_lossy(bytes).into_owned(),
        "iso-8859-1" | "latin1" => bytes.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn cow_to_decoded_string(cow: &[u8]) -> String {
    decode_mime_words(&String::from_utf8_lossy(cow))
}

fn parse_addresses(addrs: &Option<Vec<async_imap::imap_proto::types::Address<'_>>>) -> Vec<EmailAddress> {
    let Some(addrs) = addrs else { return Vec::new() };
    addrs
        .iter()
        .filter_map(|a| {
            let mailbox = a.mailbox.as_ref()?;
            let host = a.host.as_ref()?;
            let address = format!("{}@{}", String::from_utf8_lossy(mailbox), String::from_utf8_lossy(host));
            let name = a.name.as_ref().map(|c| cow_to_decoded_string(c)).filter(|s| !s.is_empty());
            Some(EmailAddress { name, address })
        })
        .collect()
}

/// Builds an [`EmailSummary`] from a `UID FETCH ... (UID FLAGS ENVELOPE
/// RFC822.SIZE INTERNALDATE BODYSTRUCTURE)` response item. `thread_key` is
/// left as a placeholder (`ThreadKey(String::new())`) - callers compute it in
/// bulk across the whole fetched set via
/// `lookout_core::thread::compute_thread_keys`. `BODYSTRUCTURE` rides along
/// so `structure`/`has_attachment` are known without any body fetch; a server
/// that omits it leaves `structure` as `None` and the viewer falls back to a
/// whole-message fetch.
pub fn summary_from_fetch(mailbox: &MailboxId, fetch: &Fetch) -> Option<EmailSummary> {
    let uid = Uid(fetch.uid?);
    let envelope = fetch.envelope()?;

    let subject = envelope.subject.as_ref().map(|c| cow_to_decoded_string(c));
    let message_id = envelope.message_id.as_ref().map(|c| String::from_utf8_lossy(c).trim().to_string());
    let in_reply_to = envelope.in_reply_to.as_ref().map(|c| String::from_utf8_lossy(c).trim().to_string());

    let date = envelope
        .date
        .as_ref()
        .and_then(|d| DateTime::parse_from_rfc2822(String::from_utf8_lossy(d).trim()).ok())
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| fetch.internal_date().map(|d| d.with_timezone(&Utc)))
        .unwrap_or_else(Utc::now);

    let mut flags = BTreeSet::new();
    let mut keywords = BTreeSet::new();
    for flag in fetch.flags() {
        let raw = imap_flag_to_string(&flag);
        match SystemFlagBit::from_imap_flag(&raw) {
            Some(bit) => {
                flags.insert(bit);
            }
            None if raw == "\\Recent" => {}
            None => {
                keywords.insert(raw);
            }
        }
    }

    let structure = fetch.bodystructure().map(crate::structure::parts_from_bodystructure);
    let has_attachment = structure.as_ref().map(|p| crate::structure::has_attachments(p)).unwrap_or(false);
    let has_calendar = structure.as_ref().map(|p| crate::structure::has_calendar_parts(p)).unwrap_or(false);

    Some(EmailSummary {
        uid,
        mailbox: mailbox.clone(),
        message_id,
        in_reply_to,
        references: Vec::new(), // ENVELOPE has no References; needs a BODY[HEADER.FIELDS (REFERENCES)] fetch (Phase 2).
        thread_key: ThreadKey(String::new()),
        subject,
        from: parse_addresses(&envelope.from),
        to: parse_addresses(&envelope.to),
        cc: parse_addresses(&envelope.cc),
        date,
        flags,
        keywords,
        size: fetch.size.unwrap_or(0),
        has_attachment,
        has_calendar,
        preview: None, // Requires a body fetch; filled in by `sync_mailbox`'s second phase.
        structure,
    })
}

fn imap_flag_to_string(flag: &async_imap::types::Flag<'_>) -> String {
    use async_imap::types::Flag;
    match flag {
        Flag::Seen => "\\Seen".to_string(),
        Flag::Answered => "\\Answered".to_string(),
        Flag::Flagged => "\\Flagged".to_string(),
        Flag::Deleted => "\\Deleted".to_string(),
        Flag::Draft => "\\Draft".to_string(),
        Flag::Recent => "\\Recent".to_string(),
        Flag::MayCreate => "\\*".to_string(),
        Flag::Custom(s) => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf8_b_encoded_subject() {
        // "Café ☕" in UTF-8, base64-encoded.
        let encoded = "=?UTF-8?B?Q2Fmw6kg4piV?=";
        assert_eq!(decode_mime_words(encoded), "Café ☕");
    }

    #[test]
    fn decodes_q_encoded_subject_with_underscores_as_spaces() {
        let encoded = "=?UTF-8?Q?Hello_World?=";
        assert_eq!(decode_mime_words(encoded), "Hello World");
    }

    #[test]
    fn leaves_plain_ascii_subject_untouched() {
        assert_eq!(decode_mime_words("Plain subject"), "Plain subject");
    }

    #[test]
    fn decodes_adjacent_encoded_words_without_inserting_fold_whitespace() {
        let encoded = "=?UTF-8?Q?Hello?= =?UTF-8?Q?World?=";
        assert_eq!(decode_mime_words(encoded), "HelloWorld");
    }

    #[test]
    fn decodes_latin1_q_encoded_subject() {
        // "café" in ISO-8859-1: c a f =E9
        let encoded = "=?ISO-8859-1?Q?caf=E9?=";
        assert_eq!(decode_mime_words(encoded), "café");
    }

    #[test]
    fn custom_flags_map_to_keyword_atoms() {
        use async_imap::types::Flag;
        // The `summary_from_fetch` keyword path: a `Flag::Custom` atom is
        // rendered verbatim, and since it's not a `SystemFlagBit` (and not
        // `\Recent`) it lands in `EmailSummary::keywords` untouched - which is
        // what lets `$Lookout-tag-*` color tags flow through the envelope
        // fetch into the cache and UI for free.
        assert_eq!(imap_flag_to_string(&Flag::Custom("$Lookout-tag-work".into())), "$Lookout-tag-work");
        assert_eq!(imap_flag_to_string(&Flag::Custom("mykeyword".into())), "mykeyword");
        // System flags still resolve to their bits.
        assert_eq!(SystemFlagBit::from_imap_flag(&imap_flag_to_string(&Flag::Seen)), Some(SystemFlagBit::Seen));
        assert_eq!(SystemFlagBit::from_imap_flag(&imap_flag_to_string(&Flag::Custom("$Lookout-tag-work".into()))), None);
    }
}
