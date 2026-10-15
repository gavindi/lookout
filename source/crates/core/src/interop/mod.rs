//! Outlook interop layer: the single home for Microsoft's wire-level strings
//! and mapping tables (see OUTLOOK_PARITY.md §2.4 decision 3 and §10).
//!
//! Everything here is pure, I/O-free string logic, matching `lookout-core`'s
//! charter (no tokio, no zbus, no gtk). The point is that feature code never
//! hard-codes Microsoft-specific header names, `X-MICROSOFT-*` properties or
//! Graph enum values: it calls into these modules instead, so a mapping only
//! ever lives in one place.
//!
//! - [`contacts_csv`] — Outlook's "Comma Separated Values" contact file
//!   (§4.6, P-08).
//! - [`headers`] — message importance and sensitivity (§4.1).
//! - [`cdo`] — the `X-MICROSOFT-CDO-*` calendar properties (§4.3).
//! - [`categories`] — Outlook's 25 category colour presets (§4.4, UX-85).
//! - [`flags`] — follow-up flag semantics (§4.1, §4.4, M-10).
//! - [`junk`] — the RFC 5788 `$Junk` / `$NotJunk` keywords (§4.2, M-17).
//! - [`mentions`] — `@mentions` detection in a message body (§5.1, M-25).
//! - [`onlinemeeting`] — Teams/Zoom/Meet join-URL detection (§4.3, CAL-20).
//! - [`snooze`] — the snooze folder and wake-time keyword (§4.2, M-11).
//! - [`tnef`] — the TNEF (`winmail.dat`) decoder, RTF decompression and HTML
//!   de-encapsulation (§4.6, M-45).
//!
//! Decoders for Outlook's remaining file formats (`.msg`, PST) are a later
//! roadmap item (1.22, 7.1) and are not here yet.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
pub mod categories;
pub mod cdo;
pub mod contacts_csv;
pub mod flags;
pub mod headers;
pub mod junk;
pub mod mentions;
pub mod onlinemeeting;
pub mod snooze;
pub mod tnef;

pub use categories::{graph_preset, nearest_preset_for_rgb, preset_by_graph, preset_by_index, preset_by_name, CategoryPreset, OUTLOOK_CATEGORY_PRESETS};
pub use cdo::{disallow_counter, BusyStatus};
pub use contacts_csv::{ContactsCsvError, OUTLOOK_CSV_COLUMNS};
pub use flags::{
    flag_due_for_date, flag_due_from_keyword, flag_due_keyword, flag_keywords, follow_up_from_headers, follow_up_from_keywords, follow_up_headers, follow_up_keywords,
    is_flag_keyword, FlagStatus, FollowUpFlag, FLAG_DONE_KEYWORD, FLAG_DUE_KEYWORD_PREFIX, FLAG_KEYWORD,
};
pub use headers::{
    auth_results_from_headers, importance_from_headers, importance_headers, sensitivity_from_headers, sensitivity_header, Importance, MessageSensitivity,
    AUTHENTICATION_RESULTS_HEADER,
};
pub use junk::{junk_keyword, junk_keyword_change, junk_keywords, JUNK_KEYWORD, NOT_JUNK_KEYWORD};
pub use mentions::MentionMatcher;
pub use onlinemeeting::{
    classify as classify_online_meeting, detect as detect_online_meeting, is_teams_meeting, OnlineMeeting, OnlineMeetingProvider, X_MICROSOFT_ONLINEMEETINGCONFLINK,
    X_MICROSOFT_SKYPETEAMSMEETINGURL,
};
pub use snooze::{snooze_keyword, snooze_keywords, snooze_until_from_keyword, snooze_until_from_keywords, SCHEDULED_FOLDER_NAME, SNOOZE_KEYWORD_PREFIX};
pub use tnef::{is_tnef_content_type, tnef_attachment_ref, tnef_part_number, TnefAttachment, TnefContents, TnefError, TNEF_CONTENT_TYPE};
