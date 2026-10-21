//! The identity model for the Graph mail transport (MAIL_GRAPH_OVERVIEW.md
//! item 5.7.3, A-06): how Graph's ids become the IMAP-shaped ids the rest of
//! Lookout keys on.
//!
//! - **`MailboxId`** is `"{account_id}:{graph folder id}"`, built by
//!   [`MailboxId::new`] like an IMAP folder's. It is parsed back **only** by
//!   stripping the known account prefix ([`graph_folder_id`]), never by
//!   splitting on `:`, because `other:`/`shared:` account ids contain one.
//! - **`Uid`** is synthetic: `lookout_mail::Cache::graph_uids` hands out one
//!   per Graph immutable id and keeps it. Uids are unique per *account*, so a
//!   moved message keeps its Uid and a copy (a new Graph id) gets a new one.
//!   [`allocation_order`] puts new messages oldest-first so Uid order follows
//!   arrival, as it does on IMAP.
//! - **`UidValidity`** is one value per account
//!   (`lookout_mail::Cache::graph_uid_validity`), fixed when the Uid map is
//!   created. It changes only when the map is lost; an expired delta cursor
//!   resyncs under the same Uids.
//! - **`BodyPart::part_number`** is the Graph attachment id in a filename-safe
//!   form ([`graph_part_number`]), since the cache names attachment files after
//!   the part number.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use lookout_core::{AccountId, MailboxId};

use super::GraphMessage;

/// The `MailboxId` of a Graph folder in `account_id`'s mailbox.
pub fn graph_mailbox_id(account_id: &AccountId, folder_id: &str) -> MailboxId {
    MailboxId::new(account_id, folder_id)
}

/// The Graph folder id inside `mailbox`, or `None` when the mailbox belongs to
/// another account. Strips the `"{account_id}:"` prefix exactly as
/// `lookout_mail::mailbox_name::mailbox_path` does (this crate doesn't depend
/// on `lookout-mail`).
pub fn graph_folder_id<'a>(account_id: &AccountId, mailbox: &'a MailboxId) -> Option<&'a str> {
    mailbox.0.strip_prefix(&format!("{}:", account_id.0))
}

/// A Graph attachment id as a `BodyPart::part_number`. Graph ids are standard
/// base64, whose `/` would turn the cache's attachment filename
/// (`…-{part_number}.bin`) into a path; this swaps to the base64url alphabet
/// (`+` → `-`, `/` → `_`), which [`attachment_id_from_part`] reverses. `-` is
/// harmless in the filename: the cache's purge sweep reads only the first
/// three `-`-separated fields.
pub fn graph_part_number(attachment_id: &str) -> String {
    attachment_id.replace('+', "-").replace('/', "_")
}

/// The Graph attachment id behind a [`graph_part_number`], or `None` for a
/// part number that couldn't have come from one (an IMAP section path such as
/// `1.2` is still a valid id shape, so this only rejects the empty string and
/// characters outside base64url).
pub fn attachment_id_from_part(part_number: &str) -> Option<String> {
    let valid = !part_number.is_empty() && part_number.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '='));
    valid.then(|| part_number.replace('-', "+").replace('_', "/"))
}

/// Orders messages for Uid allocation: oldest first by arrival, then by Graph
/// id so the order is total. Messages with no arrival time go first.
pub fn allocation_order(messages: &mut [GraphMessage]) {
    messages.sort_by(|a, b| a.received.cmp(&b.received).then_with(|| a.graph_id.cmp(&b.graph_id)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::{Importance, InferenceClassification};

    fn message(id: &str, received: Option<&str>) -> GraphMessage {
        GraphMessage {
            graph_id: id.to_string(),
            change_key: None,
            folder_id: None,
            conversation_id: None,
            message_id: None,
            subject: None,
            preview: None,
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            received: received.map(|at| at.parse().unwrap()),
            is_read: false,
            is_draft: false,
            has_attachments: false,
            is_meeting: false,
            flag: None,
            categories: Vec::new(),
            importance: Importance::Normal,
            classification: InferenceClassification::Focused,
        }
    }

    #[test]
    fn mailbox_ids_round_trip_even_when_the_account_id_has_a_colon() {
        for account in ["/org/gnome/OnlineAccounts/Accounts/account_7", "shared:abc", "other:x"] {
            let account = AccountId(account.to_string());
            let mailbox = graph_mailbox_id(&account, "AAMkAGINBOX=");
            assert_eq!(mailbox.0, format!("{}:AAMkAGINBOX=", account.0));
            assert_eq!(graph_folder_id(&account, &mailbox), Some("AAMkAGINBOX="));
        }
        let mailbox = graph_mailbox_id(&AccountId("shared:abc".into()), "f1");
        assert_eq!(graph_folder_id(&AccountId("shared".into()), &mailbox), Some("abc:f1"), "a prefix match is all it checks");
        assert_eq!(graph_folder_id(&AccountId("other:x".into()), &mailbox), None);
    }

    #[test]
    fn part_numbers_are_filename_safe_and_reversible() {
        for id in ["AAMkAGATT1=", "AAMk+AG/ATT2==", "a+b/c+d/e="] {
            let part = graph_part_number(id);
            assert!(!part.contains('/'), "{part}");
            assert!(!part.contains('+'), "{part}");
            assert_eq!(attachment_id_from_part(&part).as_deref(), Some(id));
        }
        assert_eq!(graph_part_number("AAMk+AG/ATT2=="), "AAMk-AG_ATT2==");
        assert_eq!(attachment_id_from_part(""), None);
        assert_eq!(attachment_id_from_part("../etc"), None);
    }

    #[test]
    fn allocation_order_is_oldest_first_with_the_id_as_tie_break() {
        let mut messages = vec![
            message("c", Some("2026-10-14T10:00:00Z")),
            message("b", Some("2026-10-14T09:00:00Z")),
            message("a", Some("2026-10-14T10:00:00Z")),
            message("z", None),
        ];
        allocation_order(&mut messages);
        let ids: Vec<&str> = messages.iter().map(|m| m.graph_id.as_str()).collect();
        assert_eq!(ids, ["z", "b", "a", "c"]);
    }
}
