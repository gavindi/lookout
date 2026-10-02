//! IMAP mailbox names and paths.
//!
//! Two halves. The wire encoding: IMAP carries non-ASCII mailbox names in
//! "modified UTF-7" (RFC 3501 §5.1.3) - printable ASCII stands for itself
//! (with `&` escaped as `&-`), and everything else is UTF-16BE, base64'd with
//! `,` in place of `/` and no padding, between `&` and `-`. [`encode`] turns a
//! name the user typed into that form for `CREATE`/`RENAME`, and [`decode`]
//! turns a `LIST`ed name back into something displayable. Mailbox *ids* keep
//! the raw wire path (see `MailboxId`), so only display names are decoded.
//!
//! And the path arithmetic behind the folder pane's create/rename/move/delete
//! actions: building a child or sibling path, picking a collision-free name
//! under Trash, finding the personal-namespace prefix new top-level folders
//! belong under, and re-keying an id (and its descendants) after a rename.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license

use base64::alphabet::Alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine;
use lookout_core::{AccountId, Mailbox, MailboxId, MailboxRole};

/// RFC 3501's modified base64: the standard alphabet with `,` for `/`, no
/// padding written, and none expected on the way back in.
fn modified_base64() -> GeneralPurpose {
    let alphabet = Alphabet::new("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,").expect("valid modified-base64 alphabet");
    GeneralPurpose::new(
        &alphabet,
        GeneralPurposeConfig::new()
            .with_encode_padding(false)
            .with_decode_padding_mode(DecodePaddingMode::Indifferent),
    )
}

/// Encodes `name` (one path segment or a whole path) as modified UTF-7.
pub fn encode(name: &str) -> String {
    let engine = modified_base64();
    let mut out = String::with_capacity(name.len());
    let mut pending: Vec<u16> = Vec::new();
    let flush = |pending: &mut Vec<u16>, out: &mut String| {
        if pending.is_empty() {
            return;
        }
        let bytes: Vec<u8> = pending.iter().flat_map(|unit| unit.to_be_bytes()).collect();
        out.push('&');
        out.push_str(&engine.encode(bytes));
        out.push('-');
        pending.clear();
    };
    for ch in name.chars() {
        if (' '..='~').contains(&ch) {
            flush(&mut pending, &mut out);
            if ch == '&' {
                out.push_str("&-");
            } else {
                out.push(ch);
            }
        } else {
            let mut units = [0u16; 2];
            pending.extend_from_slice(ch.encode_utf16(&mut units));
        }
    }
    flush(&mut pending, &mut out);
    out
}

/// Decodes a modified-UTF-7 name. Anything malformed (an unterminated or
/// undecodable `&...-` run) is passed through verbatim rather than dropped, so
/// a server that sends a raw non-UTF-7 name still shows *something* sensible.
pub fn decode(name: &str) -> String {
    let engine = modified_base64();
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('-') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let run = &after[..end];
        if run.is_empty() {
            out.push('&');
        } else {
            let decoded = engine.decode(run).ok().filter(|bytes| bytes.len() % 2 == 0).and_then(|bytes| {
                let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
                String::from_utf16(&units).ok()
            });
            match decoded {
                Some(text) => out.push_str(&text),
                None => out.push_str(&rest[start..start + 1 + end + 1]),
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The wire path of `mailbox` (its id minus the `"<account>:"` prefix).
pub fn mailbox_path<'a>(account_id: &AccountId, mailbox: &'a MailboxId) -> Option<&'a str> {
    mailbox.0.strip_prefix(&format!("{}:", account_id.0))
}

/// The last path segment of `path`, still wire-encoded.
pub fn leaf(path: &str, delimiter: char) -> &str {
    path.rsplit(delimiter).next().unwrap_or(path)
}

/// The path of a folder named `name` (as typed - encoded here) directly
/// under `parent_path`, or under `top_prefix` (see [`top_level_prefix`]) when
/// there is no parent.
pub fn child_path(parent_path: Option<&str>, top_prefix: &str, delimiter: char, name: &str) -> String {
    match parent_path {
        Some(parent) => format!("{parent}{delimiter}{}", encode(name)),
        None => format!("{top_prefix}{}", encode(name)),
    }
}

/// `path` with its last segment replaced by `new_name` (as typed - encoded
/// here), keeping it under the same parent.
pub fn renamed_path(path: &str, delimiter: char, new_name: &str) -> String {
    match path.rfind(delimiter) {
        Some(split) => format!("{}{delimiter}{}", &path[..split], encode(new_name)),
        None => encode(new_name),
    }
}

/// The path a folder whose wire leaf is `leaf_name` lands at when moved under
/// `trash_path`, suffixed " (2)", " (3)", ... while that name is already taken
/// (`existing` holds every known wire path).
pub fn trash_target_path(trash_path: &str, delimiter: char, leaf_name: &str, existing: &[&str]) -> String {
    let base = format!("{trash_path}{delimiter}{leaf_name}");
    if !existing.contains(&base.as_str()) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}{}", encode(&format!(" ({n})"))))
        .find(|candidate| !existing.contains(&candidate.as_str()))
        .expect("an unbounded counter always finds a free name")
}

/// Where a new *top-level* folder's path starts. Without `NAMESPACE` support
/// in the IMAP client this is inferred from the folders that already exist:
/// on servers whose personal namespace is `INBOX.` (Courier, some Cyrus and
/// Dovecot set-ups) every custom folder lives under the Inbox, and a folder
/// created at the root would be refused - so when every custom folder sits
/// under `INBOX<delim>`, new ones go there too. Otherwise, the root.
pub fn top_level_prefix(account_id: &AccountId, folders: &[Mailbox]) -> String {
    let Some(inbox) = folders.iter().find(|m| m.role == MailboxRole::Inbox) else {
        return String::new();
    };
    let Some(inbox_path) = mailbox_path(account_id, &inbox.id) else {
        return String::new();
    };
    let prefix = format!("{inbox_path}{}", inbox.delimiter);
    let mut custom = folders
        .iter()
        .filter(|m| m.role == MailboxRole::Custom)
        .filter_map(|m| mailbox_path(account_id, &m.id))
        .peekable();
    if custom.peek().is_some() && custom.all(|path| path.starts_with(&prefix)) {
        prefix
    } else {
        String::new()
    }
}

/// The folder directly above `id`, worked out from its *path* - `None` for a
/// top-level folder. Never split the raw id: GOA account ids are D-Bus object
/// paths, full of `/`, ahead of the `:` that starts the folder path.
pub fn parent_mailbox_id(account_id: &AccountId, id: &MailboxId, delimiter: char) -> Option<MailboxId> {
    let (parent, _) = mailbox_path(account_id, id)?.rsplit_once(delimiter)?;
    Some(MailboxId::new(account_id, parent))
}

/// The displayable (decoded) last segment of `id`'s path - its folder name.
pub fn display_leaf(account_id: &AccountId, id: &MailboxId, delimiter: char) -> String {
    decode(leaf(mailbox_path(account_id, id).unwrap_or(&id.0), delimiter))
}

/// Whether `candidate` is `ancestor` itself or lies anywhere beneath it.
pub fn is_same_or_descendant(candidate: &MailboxId, ancestor: &MailboxId, delimiter: char) -> bool {
    candidate == ancestor || candidate.0.strip_prefix(&ancestor.0).is_some_and(|rest| rest.starts_with(delimiter))
}

/// The id `id` becomes once the folder `from` is renamed to `to`: `to` itself
/// for an exact match, `to` plus the same tail for a descendant, and `None`
/// for anything the rename doesn't touch - including a sibling that merely
/// shares a prefix (`Work2` is not under `Work`).
pub fn rekey_mailbox_id(id: &MailboxId, from: &MailboxId, to: &MailboxId, delimiter: char) -> Option<MailboxId> {
    if id == from {
        return Some(to.clone());
    }
    let rest = id.0.strip_prefix(&from.0)?;
    rest.starts_with(delimiter).then(|| MailboxId(format!("{}{rest}", to.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox(account: &AccountId, path: &str, role: MailboxRole) -> Mailbox {
        Mailbox {
            id: MailboxId::new(account, path),
            account_id: account.clone(),
            name: leaf(path, '.').to_string(),
            parent: None,
            delimiter: '.',
            role,
            uidvalidity: lookout_core::UidValidity(1),
            uidnext: 1,
            highest_modseq: None,
            total: 0,
            unread: 0,
            flags: Vec::new(),
            subscribed: true,
        }
    }

    #[test]
    fn ascii_passes_through_except_the_ampersand() {
        assert_eq!(encode("Work Stuff"), "Work Stuff");
        assert_eq!(encode("Tom & Jerry"), "Tom &- Jerry");
        assert_eq!(decode("Tom &- Jerry"), "Tom & Jerry");
    }

    #[test]
    fn non_ascii_matches_the_rfc_examples_and_round_trips() {
        // RFC 3501 §5.1.3's own example.
        assert_eq!(decode("~peter/mail/&U,BTFw-/&ZeVnLIqe-"), "~peter/mail/台北/日本語");
        assert_eq!(encode("~peter/mail/台北/日本語"), "~peter/mail/&U,BTFw-/&ZeVnLIqe-");
        for name in ["Café", "Ünïcödé & more", "日本語", "emoji 📬 box", "a&b"] {
            assert_eq!(decode(&encode(name)), name, "{name} round-trips");
        }
        assert_eq!(encode("Café"), "Caf&AOk-");
    }

    #[test]
    fn malformed_runs_are_passed_through() {
        assert_eq!(decode("Broken &AOk"), "Broken &AOk", "an unterminated run is kept verbatim");
        assert_eq!(decode("Odd &A-"), "Odd &A-", "an undecodable run is kept verbatim");
    }

    #[test]
    fn child_and_renamed_paths_encode_the_new_segment() {
        assert_eq!(child_path(Some("Projects"), "", '/', "Café"), "Projects/Caf&AOk-");
        assert_eq!(child_path(None, "INBOX.", '.', "New"), "INBOX.New");
        assert_eq!(child_path(None, "", '/', "New"), "New");
        assert_eq!(renamed_path("Projects/Old", '/', "New"), "Projects/New");
        assert_eq!(renamed_path("Old", '/', "New"), "New");
    }

    #[test]
    fn trash_target_path_avoids_collisions() {
        assert_eq!(trash_target_path("Trash", '/', "Old", &["Inbox", "Trash"]), "Trash/Old");
        assert_eq!(trash_target_path("Trash", '/', "Old", &["Trash/Old"]), "Trash/Old (2)");
        assert_eq!(trash_target_path("Trash", '/', "Old", &["Trash/Old", "Trash/Old (2)"]), "Trash/Old (3)");
    }

    #[test]
    fn top_level_prefix_follows_an_inbox_namespace() {
        let account = AccountId("acc".to_string());
        let inbox_namespaced = vec![
            mailbox(&account, "INBOX", MailboxRole::Inbox),
            mailbox(&account, "INBOX.Sent", MailboxRole::Sent),
            mailbox(&account, "INBOX.Work", MailboxRole::Custom),
        ];
        assert_eq!(top_level_prefix(&account, &inbox_namespaced), "INBOX.");
        let flat = vec![mailbox(&account, "INBOX", MailboxRole::Inbox), mailbox(&account, "Work", MailboxRole::Custom)];
        assert_eq!(top_level_prefix(&account, &flat), "");
        let no_custom = vec![mailbox(&account, "INBOX", MailboxRole::Inbox)];
        assert_eq!(top_level_prefix(&account, &no_custom), "", "nothing to infer from: the root");
    }

    #[test]
    fn parents_and_names_come_from_the_path_not_the_account_id() {
        // A GOA account id is an object path: splitting the whole id on '/'
        // would wander into it.
        let account = AccountId("/org/gnome/OnlineAccounts/Accounts/account_1".to_string());
        let top = MailboxId::new(&account, "Work");
        let child = MailboxId::new(&account, "Work/Caf&AOk-");
        assert_eq!(parent_mailbox_id(&account, &top, '/'), None);
        assert_eq!(parent_mailbox_id(&account, &child, '/'), Some(top.clone()));
        assert_eq!(display_leaf(&account, &top, '/'), "Work");
        assert_eq!(display_leaf(&account, &child, '/'), "Café");
    }

    #[test]
    fn rekey_moves_the_folder_and_its_descendants_only() {
        let from = MailboxId("acc:Work".to_string());
        let to = MailboxId("acc:Archive/Work".to_string());
        assert_eq!(rekey_mailbox_id(&from, &from, &to, '/'), Some(to.clone()));
        assert_eq!(
            rekey_mailbox_id(&MailboxId("acc:Work/2026".to_string()), &from, &to, '/'),
            Some(MailboxId("acc:Archive/Work/2026".to_string()))
        );
        assert_eq!(
            rekey_mailbox_id(&MailboxId("acc:Work2".to_string()), &from, &to, '/'),
            None,
            "a shared prefix isn't a descendant"
        );
        assert_eq!(rekey_mailbox_id(&MailboxId("acc:Inbox".to_string()), &from, &to, '/'), None);
        assert!(is_same_or_descendant(&MailboxId("acc:Work/a/b".to_string()), &from, '/'));
        assert!(!is_same_or_descendant(&MailboxId("acc:Workshop".to_string()), &from, '/'));
    }
}
