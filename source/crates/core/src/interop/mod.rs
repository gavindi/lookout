//! Outlook interop layer: the single home for Microsoft's wire-level strings
//! and mapping tables (see OUTLOOK_PARITY.md §2.4 decision 3 and §10).
//!
//! Everything here is pure, I/O-free string logic, matching `lookout-core`'s
//! charter (no tokio, no zbus, no gtk). The point is that feature code never
//! hard-codes Microsoft-specific header names, `X-MICROSOFT-*` properties or
//! Graph enum values: it calls into these modules instead, so a mapping only
//! ever lives in one place.
//!
//! - [`headers`] — message importance and sensitivity (§4.1).
//! - [`cdo`] — the `X-MICROSOFT-CDO-*` calendar properties (§4.3).
//! - [`categories`] — Outlook's 25 category colour presets (§4.4, UX-85).
//! - [`flags`] — follow-up flag semantics (§4.1, §4.4, M-10).
//! - [`snooze`] — the snooze folder and wake-time keyword (§4.2, M-11).
//!
//! Decoders for Outlook's file formats (TNEF, `.msg`, PST) are a later roadmap
//! item (1.21–1.22, 7.1) and are not here yet.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
pub mod categories;
pub mod cdo;
pub mod flags;
pub mod headers;
pub mod snooze;

pub use categories::{graph_preset, nearest_preset_for_rgb, preset_by_graph, CategoryPreset, OUTLOOK_CATEGORY_PRESETS};
pub use cdo::BusyStatus;
pub use flags::{flag_due_from_keyword, flag_due_keyword, follow_up_from_headers, follow_up_headers, FlagStatus, FollowUpFlag, FLAG_DONE_KEYWORD, FLAG_DUE_KEYWORD_PREFIX};
pub use headers::{importance_from_headers, importance_headers, sensitivity_from_headers, sensitivity_header, Importance, MessageSensitivity};
pub use snooze::{snooze_keyword, snooze_keywords, snooze_until_from_keyword, snooze_until_from_keywords, SCHEDULED_FOLDER_NAME, SNOOZE_KEYWORD_PREFIX};
