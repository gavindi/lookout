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
}

/// One instance to actually render on a calendar view - either a
/// non-recurring [`CalendarEvent`] as-is, or one expansion of a recurring
/// event's `RRULE`. Deliberately thinner than `CalendarEvent` (no
/// description/location) since it's what gets built in bulk for a month grid.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventOccurrence {
    pub uid: EventUid,
    pub calendar_id: CalendarId,
    pub summary: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
}
