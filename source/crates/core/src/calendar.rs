use chrono::{DateTime, Utc};

use crate::email::EmailAddress;
use crate::ids::{AccountId, CalendarId, EventUid, TaskUid};

/// One invitee on an event. Structured (not a raw `ATTENDEE` string) since
/// the editor needs each address individually to render/edit attendee chips,
/// unlike `rrule`, which stays an opaque round-tripped string because it's
/// edited through a dedicated series builder instead of field-by-field.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Attendee {
    pub address: EmailAddress,
    #[serde(default)]
    pub role: AttendeeRole,
    #[serde(default)]
    pub status: AttendeeStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum AttendeeRole {
    #[default]
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum AttendeeStatus {
    #[default]
    NeedsAction,
    Accepted,
    Declined,
    Tentative,
}

/// A user-subscribed webcal (`.ics` feed) calendar: fetch-only by design -
/// feeds can't be written back to, so these never appear as event-editor
/// targets. Persisted in `settings.json` (`AppConfig`), keyed by `id` so the
/// subscription URL can change without churning its cache/colors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebcalSubscription {
    /// Stable opaque id (a UUID), the identity used everywhere outside this
    /// struct: `CalendarId("webcal:<id>")`, cache filename, color map key.
    pub id: String,
    pub display_name: String,
    /// The raw feed URL as the user typed it; `webcal://`/`webcals://`
    /// schemes are normalized to http(s) at fetch time, never rewritten here.
    pub url: String,
}

/// RFC 5545 `CLASS`: who else can see this event's details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum EventSensitivity {
    #[default]
    Public,
    Private,
    Confidential,
}

/// RFC 5545 `TRANSP`: whether this event blocks other bookings at the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum EventTransparency {
    #[default]
    Busy,
    Free,
}

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
    /// Whether the server's `supported-calendar-component-set` (RFC 4791
    /// §5.2.3) lists `VTODO`, i.e. whether tasks can be stored here. Some
    /// servers (Google Calendar) only support `VEVENT` and reject `VTODO`
    /// PUTs with HTTP 403. `true` when the server advertises `VTODO`, or when
    /// it omits the property entirely (a non-compliant server is assumed to
    /// take tasks rather than refused - the sync then fails loudly per
    /// collection instead of silently hiding the calendar's tasks).
    #[serde(default)]
    pub supports_tasks: bool,
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
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    /// The event's organizer, emitted as `ORGANIZER` only when `attendees`
    /// is non-empty (RFC 5545 requires an organizer whenever there are
    /// attendees). `None` when the owning account's own address couldn't be
    /// determined - a documented non-conformance rather than a blocker.
    #[serde(default)]
    pub organizer: Option<EmailAddress>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub sensitivity: EventSensitivity,
    #[serde(default)]
    pub transparency: EventTransparency,
    /// Minutes before `start` a `VALARM` (DISPLAY) should fire. `None` means
    /// no reminder. Only a single alarm is modeled - multiple/repeating
    /// `VALARM`s are out of scope for this pass.
    #[serde(default)]
    pub reminder_minutes_before: Option<i64>,
    /// A join-this-meeting URL (RFC 7986 `CONFERENCE`, falling back to a bare
    /// `URL` on read for interop) - the generic stand-in for
    /// provider-specific "Teams meeting" integration this app doesn't have.
    #[serde(default)]
    pub conference_url: Option<String>,
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
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    #[serde(default)]
    pub organizer: Option<EmailAddress>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub sensitivity: EventSensitivity,
    #[serde(default)]
    pub transparency: EventTransparency,
    #[serde(default)]
    pub reminder_minutes_before: Option<i64>,
    #[serde(default)]
    pub conference_url: Option<String>,
}

/// RFC 5545 §3.8.1.11 `STATUS` on a VTODO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum TaskStatus {
    #[default]
    NeedsAction,
    InProgress,
    Completed,
    Cancelled,
}

/// RFC 5545 §3.8.1.9 `PRIORITY` on a VTODO, interpreted per the RFC's own
/// scale: 1-4 high, 5 medium, 6-9 low. Modeled as the raw integer (0 = no
/// priority set, the RFC's default) so a round-trip preserves what the server
/// stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TaskPriority(pub u8);

/// One VTODO as it exists on the server. The CalDAV mirror of
/// [`CalendarEvent`], carrying the same href/etag write metadata; unlike an
/// event a task has no start/end span - `due` (or, rarely, `start` +
/// `duration`) is its only temporal anchor, so recurrence is left unmodeled
/// (RFC 5545 allows `RRULE` on VTODO but `icalendar`'s `Todo::repeats` is
/// unimplemented, and per-occurrence task expansion is out of scope).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CalendarTask {
    pub uid: TaskUid,
    pub calendar_id: CalendarId,
    pub summary: Option<String>,
    pub description: Option<String>,
    /// RFC 5545 §3.8.2.3: when the task is due. Absent for a task with only
    /// a start/duration anchor (or none at all). Server round-trip is
    /// lossless: `DUE` is written back as `DUE`, a task that came in with
    /// `DTSTART`+`DURATION` keeps its computed due time.
    #[serde(default)]
    pub due: Option<DateTime<Utc>>,
    /// RFC 5545 §3.8.2.2: when the task starts. Only meaningful when a task
    /// has a `DURATION` instead of a `DUE` - the editor always writes `DUE`.
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    /// RFC 5545 §3.8.2.1: UTC timestamp of when the task was completed. Set
    /// alongside `status == Completed`; cleared on reopen.
    #[serde(default)]
    pub completed: Option<DateTime<Utc>>,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub priority: TaskPriority,
    /// RFC 5545 §3.8.1.8: 0-100. `None` when the server didn't store it
    /// (the RFC default is 0).
    #[serde(default)]
    pub percent_complete: Option<u8>,
    #[serde(default)]
    pub categories: Vec<String>,
    /// The resource's URL within its calendar collection, if known - the
    /// target for an update or delete (`PUT`/`DELETE`). `None` for a task
    /// that hasn't been stored yet (a fresh create). See [`CalendarEvent`]
    /// for the full semantics.
    #[serde(default)]
    pub href: Option<String>,
    /// The calendar object's `getetag` as reported by the server, if known.
    #[serde(default)]
    pub etag: Option<String>,
}
