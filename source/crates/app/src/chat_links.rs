//! Deep links the AI Chat assistant can put in a reply so the user can jump
//! straight to the email or calendar event it's talking about, rather than
//! only describing it in prose.
//!
//! The chat pane's WebView has JavaScript disabled and no `decide-policy`
//! handling of its own beyond what `window.rs` connects (see the AI-Chat
//! wiring block there), so any URI scheme survives untouched from the
//! model's markdown link straight into the rendered `<a href>` - *if* the
//! model reproduces the link byte-for-byte, which is why every payload here
//! is kept as short and plain as the target `GAction` allows: `open-event`
//! reuses `app.open-event` (registered by `reminders.rs` for calendar
//! reminder notifications) verbatim, so its payload is unavoidably a
//! percent-encoded `EventOccurrence` JSON blob; `open-message` is a new
//! action this module registers, and deliberately uses a short `<uid>:<mailbox>`
//! payload instead of JSON for the same reason (see [`open_message_link`]).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::rc::Rc;

use gtk::gio;
use gtk::gio::prelude::ActionMapExt;
use gtk::glib;
use lookout_core::{EventOccurrence, MailboxId, Uid};

const SCHEME: &str = "lookout-action";

/// The `lookout-action:open-event?...` link for one occurrence - the exact
/// same JSON `reminders::show_notification` already puts on `app.open-event`'s
/// target, so that action needs no changes to answer this link too.
pub fn open_event_link(occ: &EventOccurrence) -> String {
    let json = serde_json::to_string(occ).expect("EventOccurrence serialization cannot fail");
    build_link("open-event", &json)
}

/// The `lookout-action:open-message?...` link for one message, resolved by
/// `app.open-message` (registered below) to a mailbox + UID selection.
///
/// Deliberately `<uid>:<mailbox>` rather than a JSON object: an
/// LLM-generated reply has to reproduce this string byte-for-byte to work,
/// and a long, punctuation-heavy percent-encoded JSON blob is exactly the
/// kind of "ugly" URL some models paraphrase, truncate, or otherwise mangle
/// when writing markdown. `uid` is always numeric (never contains `:`), so
/// splitting on the *first* `:` unambiguously recovers `mailbox` even though
/// `MailboxId` itself embeds a `:` (`"{account_id}:{folder_path}"`).
pub fn open_message_link(mailbox: &MailboxId, uid: Uid) -> String {
    build_link("open-message", &format!("{}:{}", uid.0, mailbox.0))
}

fn build_link(action: &str, json: &str) -> String {
    format!("{SCHEME}:{action}?data={}", percent_encode(json))
}

/// Parses a `lookout-action:<action>?data=<...>` URI back into
/// `(action_name, decoded_json_payload)`. `None` for anything that isn't
/// this scheme (or is malformed) - the caller then falls back to treating
/// `uri` as an ordinary link.
pub fn parse(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix(SCHEME)?.strip_prefix(':')?;
    let (action, query) = rest.split_once('?')?;
    let data = query.strip_prefix("data=")?;
    if action.is_empty() {
        return None;
    }
    Some((action.to_string(), percent_decode(data)))
}

/// RFC 3986 unreserved characters pass through; everything else becomes
/// `%XX`. Same convention as `graph_pin.rs`/`google_tasks.rs` - no `url`
/// crate in this workspace.
fn percent_encode(s: &str) -> String {
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

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Registers `app.open-message` (STRING target = `<uid>:<mailbox>`, built by
/// [`open_message_link`]). Mirrors `mail_notifications::spawn_actions`'s
/// convention: this module just parses the target, the window code injects
/// the closure that actually navigates. A malformed target (should be
/// unreachable outside a hand-crafted link - most likely an LLM reply that
/// mangled the link on its way out) is logged and dropped rather than
/// panicking.
pub fn spawn_open_message_action(app: &adw::Application, open_message: Rc<dyn Fn(MailboxId, Uid)>) {
    let action = gio::SimpleAction::new("open-message", Some(glib::VariantTy::STRING));
    action.connect_activate(move |_, param| {
        let Some(target) = param.and_then(|v| v.get::<String>()) else { return };
        let Some((uid, mailbox)) = target.split_once(':').and_then(|(uid, mailbox)| Some((uid.parse::<u32>().ok()?, mailbox))) else {
            tracing::warn!("open-message action target is not <uid>:<mailbox>: {target:?}");
            return;
        };
        tracing::debug!("open-message activated: uid={uid} mailbox={mailbox:?}");
        open_message(MailboxId(mailbox.to_string()), Uid(uid));
    });
    app.add_action(&action);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lookout_core::{CalendarId, EventUid};

    fn occ() -> EventOccurrence {
        EventOccurrence {
            uid: EventUid("event-1".to_string()),
            calendar_id: CalendarId("cal-1".to_string()),
            summary: Some("Team sync".to_string()),
            description: None,
            location: None,
            start: Utc::now(),
            end: Utc::now(),
            all_day: false,
            rrule: None,
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: Default::default(),
            transparency: Default::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    #[test]
    fn percent_encode_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(percent_encode("abc-_.~"), "abc-_.~");
        assert_eq!(percent_encode("{\"a\":1}"), "%7B%22a%22%3A1%7D");
    }

    #[test]
    fn percent_decode_round_trips_through_encode() {
        let s = "hello world/with?special&chars=1";
        assert_eq!(percent_decode(&percent_encode(s)), s);
    }

    #[test]
    fn open_event_link_round_trips_to_the_same_json_reminders_uses() {
        let occurrence = occ();
        let link = open_event_link(&occurrence);
        let (action, payload) = parse(&link).expect("valid lookout-action link");
        assert_eq!(action, "open-event");
        assert_eq!(payload, serde_json::to_string(&occurrence).unwrap());
    }

    #[test]
    fn open_message_link_round_trips_mailbox_and_uid() {
        // A mailbox id that itself embeds a `:` - the case `split_once(':')`
        // must still get right, since only the *first* `:` is the delimiter.
        let mailbox = MailboxId("acct-1:INBOX/Sub".to_string());
        let link = open_message_link(&mailbox, Uid(42));
        let (action, payload) = parse(&link).expect("valid lookout-action link");
        assert_eq!(action, "open-message");
        assert_eq!(payload, "42:acct-1:INBOX/Sub");
        let (uid, parsed_mailbox) = payload.split_once(':').unwrap();
        assert_eq!(uid, "42");
        assert_eq!(parsed_mailbox, "acct-1:INBOX/Sub");
    }

    #[test]
    fn parse_rejects_other_schemes() {
        assert_eq!(parse("https://example.com"), None);
        assert_eq!(parse("mailto:someone@example.com"), None);
        assert_eq!(parse("lookout-action:"), None);
    }
}
