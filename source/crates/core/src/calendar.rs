use chrono::{DateTime, Utc};

use crate::ids::{AccountId, CalendarId, EventUid};

/// A CalDAV calendar collection discovered under an account's calendar-home-set.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CalendarInfo {
    pub id: CalendarId,
    pub account_id: AccountId,
    pub display_name: String,
    /// Hex color (e.g. `"#3584e4"`) from the CalDAV `calendar-color` extension
    /// property, if the server advertises one (an Apple/Nextcloud extension,
    /// not core RFC 4791). `None` means "pick a default accent color in the UI".
    pub color: Option<String>,
    /// The calendar collection's href, needed to build later REPORT requests
    /// against this specific calendar.
    pub href: String,
}

/// One VEVENT as it exists on the server: for a recurring event this is the
/// *master* instance (`DTSTART`/`DTEND` + `RRULE`), not an individual
/// occurrence - see [`EventOccurrence`] for the expanded/rendered form.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CalendarEvent {
    pub uid: EventUid,
    pub calendar_id: CalendarId,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    /// Raw RRULE value (RFC 5545 `RECUR` value), unparsed - parsing is
    /// `lookout-dav`'s job since this crate has no I/O deps and RRULE parsing
    /// is a heavier dependency than this crate should carry.
    pub rrule: Option<String>,
    /// The resource's URL within its calendar collection (e.g.
    /// `.../events/abc.ics`) as reported by the server, if known - the target
    /// for an update or delete (`PUT`/`DELETE`). `None` for an event that
    /// hasn't been stored yet (a fresh create).
    #[serde(default)]
    pub href: Option<String>,
    /// The calendar object's `getetag` as reported by the server, if known -
    /// passed as `If-Match` on updates/deletes so a write fails loudly
    /// (HTTP 412) instead of silently clobbering a concurrent change.
    #[serde(default)]
    pub etag: Option<String>,
}

/// One instance to actually render on a calendar view - either a
/// non-recurring [`CalendarEvent`] as-is, or one expansion of a recurring
/// event's `RRULE`. Deliberately thinner than [`CalendarEvent`] (no
/// description/location) since it's what gets built in bulk for a month grid.
///
/// Still carries enough to open an editor for the event it came from: a
/// clicked occurrence yields the summary/time plus the description, location,
/// href, etag and RRULE of its master (kept for non-recurring events and for
/// each expansion of a recurring one, so the editor doesn't need a separate
/// full-event lookup).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventOccurrence {
    pub uid: EventUid,
    pub calendar_id: CalendarId,
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    /// The master's raw RRULE (see [`CalendarEvent::rrule`]), so editing an
    /// occurrence can preserve the series it came from. `None` for a
    /// non-recurring event.
    #[serde(default)]
    pub rrule: Option<String>,
    /// The master event's anchor `DTSTART`/`DTEND` (UTC). For a non-recurring
    /// event these equal the occurrence's own times; for a recurring one they
    /// are the series anchor - an occurrence's `start`/`end` are just one
    /// expansion. An editor editing the whole series must show/keep the anchor
    /// (or deliberately change it to move the series), not the clicked
    /// occurrence's date.
    #[serde(default)]
    pub master_start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub master_end: Option<DateTime<Utc>>,
    /// The master [`CalendarEvent`]'s resource URL/etag, so the occurrence can
    /// be edited or deleted in place (see [`CalendarEvent`] for the semantics).
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
}
