//! "Reply with meeting" (OUTLOOK_PARITY.md item 3.19 / C-20).
//!
//! Derives a new event's invitees and subject from a conversation: Outlook's
//! "Reply all by meeting" opens a meeting request prefilled with everyone on
//! the thread. The derivation is pure - no GTK, no window, no network - so it
//! is unit-tested directly and reused by both the message context menu and the
//! reading pane's More menu.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use lookout_core::{EmailAddress, EmailSummary};

/// The conversation's participants, newest message first, deduplicated
/// case-insensitively and with the user's own addresses (`self_addresses`)
/// removed so a reply-with-meeting never invites the user. A display name is
/// kept from the first message that carries one for an address.
pub fn meeting_participants(messages: &[EmailSummary], self_addresses: &[String]) -> Vec<EmailAddress> {
    let self_keys: Vec<String> = self_addresses.iter().map(|address| address.trim().to_ascii_lowercase()).collect();
    let is_self = |address: &str| !address.trim().is_empty() && self_keys.contains(&address.trim().to_ascii_lowercase());

    let mut newest_first: Vec<&EmailSummary> = messages.iter().collect();
    newest_first.sort_by_key(|message| std::cmp::Reverse(message.date));

    let mut seen: Vec<String> = Vec::new();
    let mut participants: Vec<EmailAddress> = Vec::new();
    for message in newest_first {
        for address in message.from.iter().chain(message.to.iter()).chain(message.cc.iter()) {
            let key = address.address.trim().to_ascii_lowercase();
            if key.is_empty() || is_self(&address.address) || seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let name = address.name.clone().filter(|name| !name.trim().is_empty());
            participants.push(EmailAddress {
                name,
                address: address.address.trim().to_string(),
            });
        }
    }
    participants
}

/// The meeting's subject: the newest message's subject, verbatim - Outlook
/// reuses the thread subject for "Reply all by meeting". Falls back to
/// "Meeting" when the conversation carries none.
pub fn meeting_subject(messages: &[EmailSummary]) -> String {
    messages
        .iter()
        .max_by_key(|message| message.date)
        .and_then(|message| message.subject.as_deref())
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "Meeting".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lookout_core::{MailboxId, ThreadKey, Uid};

    fn message(uid: u32, hour: u32, from: &str, to: &[&str], cc: &[&str], subject: Option<&str>) -> EmailSummary {
        let address = |value: &str| EmailAddress {
            name: None,
            address: value.to_string(),
        };
        EmailSummary {
            inference_classification: Default::default(),
            uid: Uid(uid),
            mailbox: MailboxId("acct:INBOX".into()),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_key: ThreadKey(String::new()),
            subject: subject.map(str::to_string),
            from: vec![address(from)],
            to: to.iter().map(|value| address(value)).collect(),
            cc: cc.iter().map(|value| address(value)).collect(),
            date: Utc.with_ymd_and_hms(2026, 10, 15, hour, 0, 0).unwrap(),
            flags: Default::default(),
            keywords: Default::default(),
            size: 0,
            has_attachment: false,
            has_calendar: false,
            preview: None,
            mentions: false,
            importance: Default::default(),
            sensitivity: Default::default(),
            list_id: None,
            structure: None,
        }
    }

    #[test]
    fn participants_join_from_to_and_cc_newest_first() {
        let older = message(1, 8, "alice@example.com", &["me@example.com", "bob@example.com"], &[], Some("Topic"));
        let newer = message(2, 9, "carol@example.com", &["me@example.com"], &["dave@example.com"], Some("Re: Topic"));
        let participants = meeting_participants(&[older, newer], &["me@example.com".to_string()]);
        let addresses: Vec<&str> = participants.iter().map(|a| a.address.as_str()).collect();
        assert_eq!(addresses, vec!["carol@example.com", "dave@example.com", "alice@example.com", "bob@example.com"]);
    }

    #[test]
    fn participants_are_deduped_case_insensitively_and_exclude_self() {
        let first = message(1, 8, "Alice@Example.com", &["me@example.com"], &["alice@example.com"], None);
        let second = message(2, 9, "bob@example.com", &["me@example.com"], &[], None);
        let participants = meeting_participants(&[first, second], &["ME@example.com".to_string()]);
        let addresses: Vec<&str> = participants.iter().map(|a| a.address.as_str()).collect();
        assert_eq!(addresses, vec!["bob@example.com", "Alice@Example.com"]);
    }

    #[test]
    fn a_display_name_survives_from_the_first_mention() {
        let mut sender = message(1, 8, "alice@example.com", &["me@example.com"], &[], None);
        sender.from[0].name = Some("Alice Smith".to_string());
        let participants = meeting_participants(&[sender], &[]);
        assert_eq!(participants[0].name.as_deref(), Some("Alice Smith"));
    }

    #[test]
    fn subject_is_the_newest_messages_verbatim_and_defaults() {
        let older = message(1, 8, "a@example.com", &[], &[], Some("Original"));
        let newer = message(2, 9, "b@example.com", &[], &[], Some("Re: Original"));
        assert_eq!(meeting_subject(&[older.clone(), newer]), "Re: Original");
        assert_eq!(meeting_subject(&[]), "Meeting");
        let untitled = message(3, 10, "c@example.com", &[], &[], Some("   "));
        assert_eq!(meeting_subject(&[untitled]), "Meeting");
    }
}
