//! Protocol- and UI-agnostic domain types shared by every Lookout crate.
//!
//! Deliberately has no I/O dependencies (no tokio, no zbus, no gtk) so it can
//! be exercised with plain `cargo test` and reused by any future front end.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
pub mod calendar;
pub mod email;
pub mod identity;
pub mod ids;
pub mod interop;
pub mod mailbox;
pub mod signature;
pub mod thread;
pub mod trust;
pub mod vcard;

pub use calendar::{
    Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, CalendarInfo, CalendarTask, EventOccurrence, EventSensitivity, EventTransparency, RecurrenceRange, TaskPriority,
    TaskStatus, WebcalSubscription,
};
pub use email::{
    cid_matches, header_value, is_auto_submitted, is_report_message, parse_disposition_notification_to, parse_imip_method, parse_list_unsubscribe, sanitize_tag_key,
    tag_key_from_keyword, tag_keyword, AuthenticationResults, BodyPart, ContactsProvider, DkimResult, DmarcResult, EmailAddress, EmailBody, EmailSummary, ImipInvitation,
    ImipMethod, ListUnsubscribe, SpfResult, SystemFlagBit, TAG_KEYWORD_PREFIX,
};
pub use identity::Identity;
pub use ids::{AccountId, CalendarId, EventUid, MailboxId, TaskUid, Uid, UidValidity};
pub use interop::{
    flag_due_for_date, flag_due_from_keyword, flag_due_keyword, flag_keywords, follow_up_from_headers, follow_up_from_keywords, follow_up_headers, follow_up_keywords,
    graph_preset, importance_from_headers, importance_headers, is_flag_keyword, nearest_preset_for_rgb, preset_by_graph, sensitivity_from_headers, sensitivity_header, BusyStatus,
    CategoryPreset, FlagStatus, FollowUpFlag, Importance, MessageSensitivity, FLAG_DONE_KEYWORD, FLAG_DUE_KEYWORD_PREFIX, FLAG_KEYWORD, OUTLOOK_CATEGORY_PRESETS,
};
pub use interop::{junk_keyword, junk_keyword_change, junk_keywords, JUNK_KEYWORD, NOT_JUNK_KEYWORD};
pub use interop::{snooze_keyword, snooze_keywords, snooze_until_from_keyword, snooze_until_from_keywords, SCHEDULED_FOLDER_NAME, SNOOZE_KEYWORD_PREFIX};
pub use mailbox::{display_name, Mailbox, MailboxRole};
pub use signature::Signature;
pub use thread::{ThreadGroup, ThreadKey};
pub use trust::{html_remote_content_scan, normalize_trust_entry, sender_matches_trust_entry, RemoteContentScan, TrustLevel};
pub use vcard::{AddressField, Birthday, EmailField, Name, OtherProperty, Parameter, TelephoneField, VCard, VCardError};
