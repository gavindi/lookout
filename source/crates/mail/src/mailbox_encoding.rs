//! IMAP modified UTF-7 is a wire encoding; mailbox IDs keep the original
//! name for commands, while the sidebar displays decoded Unicode.
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};

pub(crate) fn decode_mailbox_name(name: &str) -> String {
    let mut result = String::new();
    let mut rest = name;
    while let Some(start) = rest.find('&') {
        result.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('-') else {
            break;
        };
        let encoded = &rest[1..end];
        if encoded.is_empty() {
            result.push('&');
        } else {
            let decoded = STANDARD_NO_PAD.decode(encoded.replace(',', "/")).ok().and_then(|bytes| {
                if bytes.len() % 2 != 0 {
                    return None;
                }
                let units: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_be_bytes([b[0], b[1]])).collect();
                String::from_utf16(&units).ok()
            });
            match decoded {
                Some(decoded) => result.push_str(&decoded),
                None => result.push_str(&rest[..=end]),
            }
        }
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_umlauts_and_literal_ampersands() {
        assert_eq!(decode_mailbox_name("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(decode_mailbox_name("&AOQA9gD8-"), "äöü");
        assert_eq!(decode_mailbox_name("R&-D"), "R&D");
        assert_eq!(decode_mailbox_name("~peter/mail/&U,BTFw-/&ZeVnLIqe-"), "~peter/mail/台北/日本語");
        assert_eq!(decode_mailbox_name("&2D3eAA-"), "😀");
    }

    #[test]
    fn preserves_utf8_and_malformed_names() {
        for name in ["INBOX", "Entwürfe", "A&broken", "A&!-B", "&AA-", "&2AA-"] {
            assert_eq!(decode_mailbox_name(name), name);
        }
    }
}
