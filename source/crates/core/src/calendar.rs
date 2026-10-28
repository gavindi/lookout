/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use base64::Engine as _;
use chrono::{DateTime, Utc};

use crate::email::EmailAddress;
use crate::ids::{AccountId, CalendarId, EventUid, TaskUid};
use crate::interop::cdo::BusyStatus;

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
    /// A bookable resource (a room or equipment mailbox, CAL-19): a required
    /// participant Graph calls `resource` and iCalendar marks
    /// `CUTYPE=RESOURCE`, distinct from an ordinary required attendee so the
    /// server books it rather than inviting a person.
    Resource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum AttendeeStatus {
    #[default]
    NeedsAction,
    Accepted,
    Declined,
    Tentative,
}

/// RFC 6638 `SCHEDULE-AGENT` parameter on an iCalendar `ORGANIZER` property:
/// who delivers the invitations for the event. Only the organizer's value is
/// interpreted here - `Client` (set by a scheduling client that keeps
/// delivery in its own hands) suppresses server-side scheduling, `Server` (a
/// 6638 server's own marker) and an absent parameter leave delivery to the
/// server when it supports scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ScheduleAgent {
    Server,
    Client,
    None,
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

/// RFC 5545 §3.8.1.11 `STATUS` on a VEVENT: the event's own lifecycle state.
/// `Confirmed` is the RFC default and the value a plain event carries;
/// `Tentative` marks a provisional booking (Outlook also expresses this
/// through "Show as", but a non-Outlook source writes it here); `Cancelled`
/// means the organizer withdrew the event, so the views fade and strike it
/// through. This is independent of [`BusyStatus`] (`TRANSP`/"Show as"), which
/// only says whether the time is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum EventStatus {
    #[default]
    Confirmed,
    Tentative,
    Cancelled,
}

/// RFC 5545 `RANGE` parameter on a `RECURRENCE-ID` (§3.2.13): how far a
/// per-occurrence override reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RecurrenceRange {
    /// No `RANGE` parameter: the override replaces just this one instance.
    #[default]
    This,
    /// `RANGE=THISANDFUTURE`: the override replaces this instance and every
    /// later one in the series.
    ThisAndFuture,
}

/// RFC 5545 `TRANSP`: the binary form of an event's "Show as" state - whether
/// it blocks other bookings at the same time. Since 3.3 the model stores the
/// full five-state [`BusyStatus`] instead; this type survives only as the
/// wire-level bridge `cdo.rs` uses to derive the `TRANSP` line non-Outlook
/// clients understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum EventTransparency {
    #[default]
    Busy,
    Free,
}

/// RFC 6638 CalDAV scheduling capability, discovered from the account
/// principal (`CALDAV:schedule-outbox-URL` / `schedule-inbox-URL`). An outbox
/// URL means the server itself delivers iTIP messages for events stored in
/// its calendars (implicit scheduling), so the client must not also send its
/// own iMIP mail; a server that doesn't advertise one leaves delivery to the
/// client. `None` everywhere is the safe default for non-scheduling servers.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchedulingInfo {
    /// The account principal's `CALDAV:schedule-outbox-URL`, when advertised -
    /// where a client POSTs VFREEBUSY requests and, on a scheduling-capable
    /// server, the marker that the server handles iTIP itself.
    #[serde(default)]
    pub schedule_outbox: Option<String>,
    /// The account principal's `CALDAV:schedule-inbox-URL`, when advertised -
    /// where the server posts incoming scheduling messages; kept for the
    /// free/busy work (CAL-19) that consumes the outbox.
    #[serde(default)]
    pub schedule_inbox: Option<String>,
}

impl SchedulingInfo {
    /// Whether the server says it schedules: implicitly delivering iTIP
    /// REQUEST/REPLY/CANCEL for events written to its calendars, so the app
    /// must skip its own client-side iMIP sending (RFC 6638 §6).
    pub fn supports_server_scheduling(&self) -> bool {
        self.schedule_outbox.is_some()
    }
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
    /// The mailbox that owns this calendar, when it isn't the signed-in user -
    /// a shared or delegated calendar opened from another user's mailbox
    /// (CAL-25). Graph builds the calendar's path as
    /// `users/{owner}/calendars/{id}` (and its events as `users/{owner}/events/
    /// {id}`) when this is set, instead of the account's own `/me/...`. `None`
    /// for the account's own calendars and for every non-Graph (CalDAV) source.
    #[serde(default)]
    pub owner: Option<String>,
}

/// One attachment on an event (OUTLOOK_PARITY.md item 3.16 / CAL-21). Models
/// the three forms an RFC 5545 `ATTACH` takes:
///
/// - a server-managed CalDAV attachment (RFC 8607): a `url` plus a
///   `managed_id` (the `MANAGED-ID` parameter the server generated);
/// - a plain URI attachment: only `url`;
/// - an inline attachment (`VALUE=BINARY` / `ENCODING=BASE64`): `data` holds
///   the base64 payload, which is what iMIP invitations carry and what a
///   CalDAV server without RFC 8607 stores in the calendar object.
///
/// `name`, `mime_type` and `size` come from the RFC 8607 `FILENAME`/`FMTTYPE`/
/// `SIZE` parameters (with sensible fallbacks derived from the URI or the
/// decoded bytes).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct EventAttachment {
    /// The display file name. RFC 8607 deliberately does not call it a path -
    /// it is untrusted input, so callers sanitize it before writing to disk.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    /// The `ATTACH` value when it is a URI (or, on the Graph backend, the
    /// attachment's server id - see [`EventAttachment::graph_id`]).
    #[serde(default)]
    pub url: Option<String>,
    /// RFC 8607 `MANAGED-ID`, present only on a server-managed CalDAV
    /// attachment. Written back so the server can match the property, and
    /// stripped from the iMIP form (RFC 8607 §3.12.7: the id is meaningless
    /// outside the server that issued it).
    #[serde(default)]
    pub managed_id: Option<String>,
    /// Base64-encoded inline contents (`ATTACH;VALUE=BINARY`). Kept encoded
    /// in the model so it round-trips through the JSON cache and `settings`
    /// without a byte-array encoder; use [`Self::bytes`] to decode.
    #[serde(default)]
    pub data: Option<String>,
}

/// The largest inline attachment Lookout will carry in a calendar object or
/// an iMIP invitation. The iCalendar form is base64, so a document with this
/// much data is roughly a third larger on the wire; bigger files are refused
/// by the editor with an explanatory message rather than silently dropped.
pub const MAX_INLINE_EVENT_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;

impl EventAttachment {
    /// An inline attachment from raw bytes (what the editor's file chooser
    /// produces). Returns `None` for a file over
    /// [`MAX_INLINE_EVENT_ATTACHMENT_BYTES`].
    pub fn from_bytes(name: impl Into<String>, mime_type: Option<String>, data: &[u8]) -> Option<Self> {
        if data.len() > MAX_INLINE_EVENT_ATTACHMENT_BYTES {
            return None;
        }
        Some(Self {
            name: name.into(),
            mime_type,
            size: Some(data.len() as u64),
            url: None,
            managed_id: None,
            data: Some(base64::engine::general_purpose::STANDARD.encode(data)),
        })
    }

    /// The decoded inline contents, or `None` for a URI-only attachment (or a
    /// payload that isn't valid base64).
    pub fn bytes(&self) -> Option<Vec<u8>> {
        base64::engine::general_purpose::STANDARD.decode(self.data.as_deref()?).ok()
    }

    /// Whether this attachment's contents travel inside the calendar object.
    pub fn is_inline(&self) -> bool {
        self.data.is_some()
    }

    /// The size to display: the declared `SIZE`, else the decoded inline
    /// length.
    pub fn byte_size(&self) -> Option<u64> {
        self.size.or_else(|| self.bytes().map(|bytes| bytes.len() as u64))
    }

    /// The Graph attachment id, when this came from the Microsoft Graph
    /// backend (Graph's attachment list has no URI; the id is the handle a
    /// `GET /me/events/{id}/attachments/{id}` uses). A CalDAV `ATTACH` URI -
    /// absolute (`scheme://`), collection-relative (`/…`) or dot-relative -
    /// is distinguishable from Graph's base64 id, so the two can share `url`.
    pub fn graph_id(&self) -> Option<&str> {
        self.url.as_deref().filter(|url| {
            !url.contains("://") && !url.starts_with('/') && !url.starts_with("./") && !url.starts_with("../") && !url.contains([':', '?', '#']) && self.managed_id.is_none()
        })
    }
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
    /// The event's own IANA time zone (CAL-11), when it has one - Outlook's
    /// per-event "time zone" and the `TZID` a CalDAV or Outlook document
    /// carries. `start`/`end` stay UTC instants; this records which zone the
    /// event's wall-clock times belong to, so the editor can enter/display
    /// them there and the writer can emit `TZID` plus `VTIMEZONE`. `None`
    /// for a floating or UTC event, and for all-day events (their
    /// `VALUE=DATE` boundaries are zone-independent).
    #[serde(default)]
    pub time_zone: Option<String>,
    /// Raw RRULE value (RFC 5545 `RECUR` value), unparsed - parsing is
    /// `lookout-dav`'s job since this crate has no I/O deps and RRULE parsing
    /// is a heavier dependency than this crate should carry.
    pub rrule: Option<String>,
    /// RFC 5545 `RECURRENCE-ID` (UTC): set when this VEVENT is a
    /// *per-occurrence override* of a recurring master rather than the master
    /// itself - the instance it replaces. `None` for masters and for
    /// non-recurring events.
    #[serde(default)]
    pub recurrence_id: Option<DateTime<Utc>>,
    /// The `RANGE` parameter of the override's `RECURRENCE-ID` (see
    /// [`RecurrenceRange`]) - `ThisAndFuture` means the override also applies
    /// to every later instance of the series. Meaningless when
    /// `recurrence_id` is `None`.
    #[serde(default)]
    pub recurrence_range: RecurrenceRange,
    /// RFC 5545 `EXDATE` values (UTC): instances of the series to omit.
    /// Modeled directly (unlike `RRULE`'s raw string) since they're plain
    /// timestamps, and re-emitted verbatim on serialize - the only operation
    /// ever performed on them is add/remove.
    #[serde(default)]
    pub exdates: Vec<DateTime<Utc>>,
    /// RFC 5545 `RDATE` values (UTC): extra instances beyond the `RRULE`'s
    /// expansion. Read and round-tripped so a master's RDATEs survive a PUT;
    /// feeding them into expansion is a follow-up.
    #[serde(default)]
    pub rdates: Vec<DateTime<Utc>>,
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
    /// RFC 5545 `SEQUENCE` (default 0): the organizer bumps it on every
    /// significant revision sent to attendees, so an iTIP client can order
    /// updates and ignore stale ones. Read/written on the wire; the app
    /// increments it when an organizer saves an event with attendees.
    #[serde(default)]
    pub sequence: i64,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    /// The event's organizer, emitted as `ORGANIZER` only when `attendees`
    /// is non-empty (RFC 5545 requires an organizer whenever there are
    /// attendees). `None` when the owning account's own address couldn't be
    /// determined - a documented non-conformance rather than a blocker.
    #[serde(default)]
    pub organizer: Option<EmailAddress>,
    /// RFC 6638 `SCHEDULE-AGENT` on the `ORGANIZER` property, when present -
    /// the per-event override of who delivers invitations (see
    /// [`ScheduleAgent`]). `None` means the parameter was absent.
    #[serde(default)]
    pub schedule_agent: Option<ScheduleAgent>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub sensitivity: EventSensitivity,
    /// The item's "Show as" state (CAL-05): the full five-state Outlook/CDO
    /// form of RFC 5545's binary `TRANSP` (see [`BusyStatus`]). Serialized as
    /// `show_as`; the `transparency` alias keeps DAV-cache JSON written before
    /// 3.3 readable.
    #[serde(default, rename = "show_as", alias = "transparency")]
    pub show_as: BusyStatus,
    /// RFC 5545 §3.8.1.11 `STATUS`: the event's own lifecycle state (see
    /// [`EventStatus`]). Distinct from `show_as`, which only describes
    /// time-blocking - a `Cancelled` event is struck through and faded in the
    /// calendar views whatever its "Show as" says.
    #[serde(default)]
    pub status: EventStatus,
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
    /// The event's attachments (CAL-21): RFC 8607 managed attachments, plain
    /// URIs, or inline `ATTACH;VALUE=BINARY` payloads. Empty for an event
    /// with none.
    #[serde(default)]
    pub attachments: Vec<EventAttachment>,
    /// Whether this is an online meeting hosted by Microsoft Teams (Graph
    /// `isOnlineMeeting`). When set, the Graph writer asks Microsoft to
    /// create the meeting link (`onlineMeetingProvider: teamsForBusiness`) and
    /// the resulting join URL comes back in `conference_url`. Non-Graph
    /// backends ignore it, and `EventOccurrence` infers the state from
    /// `conference_url` rather than carrying this field.
    #[serde(default)]
    pub is_online_meeting: bool,
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
    /// The event's own IANA time zone (see [`CalendarEvent::time_zone`]).
    #[serde(default)]
    pub time_zone: Option<String>,
    /// The master's raw RRULE (see [`CalendarEvent::rrule`]), so editing an
    /// occurrence can preserve the series it came from. `None` for a
    /// non-recurring event.
    #[serde(default)]
    pub rrule: Option<String>,
    /// The `RECURRENCE-ID` of the VEVENT this occurrence came from - set when
    /// the occurrence is a per-occurrence override of a recurring series
    /// (see [`CalendarEvent::recurrence_id`]).
    #[serde(default)]
    pub recurrence_id: Option<DateTime<Utc>>,
    /// The master's `EXDATE` list (see [`CalendarEvent::exdates`]) - carried
    /// so editing an override doesn't silently drop the master's other
    /// exclusions on save.
    #[serde(default)]
    pub exdates: Vec<DateTime<Utc>>,
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
    /// For an occurrence derived from a per-occurrence override these are the
    /// *override's* resource - the master's own live at
    /// [`Self::master_href`]/[`Self::master_etag`] (when known), so a
    /// whole-series edit can be routed to the right resource from any
    /// occurrence.
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
    /// The series master's own resource URL/etag. Only set on occurrences
    /// derived from per-occurrence overrides (a plain expansion's `href`/`etag`
    /// already *are* the master's). `None` when unknown.
    #[serde(default)]
    pub master_href: Option<String>,
    #[serde(default)]
    pub master_etag: Option<String>,
    /// The master event's `SEQUENCE` (see [`CalendarEvent::sequence`]), so an
    /// organizer editing from any occurrence can bump the series revision.
    #[serde(default)]
    pub sequence: i64,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    #[serde(default)]
    pub organizer: Option<EmailAddress>,
    /// The master's `SCHEDULE-AGENT` (see [`CalendarEvent::schedule_agent`]).
    #[serde(default)]
    pub schedule_agent: Option<ScheduleAgent>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub sensitivity: EventSensitivity,
    /// The occurrence's "Show as" state (see [`CalendarEvent::show_as`]).
    #[serde(default, rename = "show_as", alias = "transparency")]
    pub show_as: BusyStatus,
    /// The occurrence's lifecycle state (see [`CalendarEvent::status`]).
    #[serde(default)]
    pub status: EventStatus,
    #[serde(default)]
    pub reminder_minutes_before: Option<i64>,
    #[serde(default)]
    pub conference_url: Option<String>,
    /// The event's attachments (see [`CalendarEvent::attachments`]).
    #[serde(default)]
    pub attachments: Vec<EventAttachment>,
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
/// `duration`) is its only temporal anchor.
///
/// A recurring task keeps its raw `RRULE` and is rolled forward in place when
/// an occurrence is completed (RFC 5545 permits exactly one VTODO per series
/// master; per-occurrence `RECURRENCE-ID` overrides are not modeled).
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
    /// For a recurring task this is also the series anchor (`DTSTART`, which
    /// RFC 5545 requires alongside `RRULE`).
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    /// Raw RFC 5545 `RECUR` value (`RRULE`), e.g. `FREQ=WEEKLY;BYDAY=MO`.
    /// `None` for a one-off task. Kept unparsed, exactly like
    /// [`CalendarEvent::rrule`], so a rule written by another client survives
    /// a round trip; `lookout-dav` expands it for roll-forward.
    #[serde(default)]
    pub rrule: Option<String>,
    /// RFC 5545 §3.8.7.1 (`CREATED`): when the task was created, as recorded
    /// by the originating client/server. Used by the Tasks view's "created"
    /// sort (T-09). `None` when the server didn't supply it.
    #[serde(default)]
    pub created: Option<DateTime<Utc>>,
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
    /// Minutes before `due` the first `VALARM` fires (T-05); `None` for no
    /// reminder. Signed so a trigger after the due time can round-trip. The
    /// event model uses the same shape (`CalendarEvent::reminder_minutes_before`).
    #[serde(default)]
    pub reminder_minutes_before: Option<i64>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inline_attachment_round_trips_its_bytes() {
        let attachment = EventAttachment::from_bytes("notes.txt", Some("text/plain".to_string()), b"Hello agenda!").unwrap();
        assert_eq!(attachment.name, "notes.txt");
        assert_eq!(attachment.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(attachment.byte_size(), Some(13));
        assert_eq!(attachment.bytes().as_deref(), Some(b"Hello agenda!".as_slice()));
        assert!(attachment.is_inline());
        assert!(attachment.graph_id().is_none());
    }

    #[test]
    fn an_oversized_inline_attachment_is_refused() {
        let oversized = vec![0u8; MAX_INLINE_EVENT_ATTACHMENT_BYTES + 1];
        assert!(EventAttachment::from_bytes("huge.bin", None, &oversized).is_none());
    }

    #[test]
    fn graph_ids_are_distinguished_from_caldav_uris() {
        let graph = EventAttachment {
            url: Some("AAMkAGI1AAAt9AHjAAABEgAQAEdBogju-MJEu6Ngg-1_W0g=".to_string()),
            ..Default::default()
        };
        assert_eq!(graph.graph_id(), Some("AAMkAGI1AAAt9AHjAAABEgAQAEdBogju-MJEu6Ngg-1_W0g="));

        for url in ["https://cal.example.com/attach/64/34X22R", "/attach/64/34X22R"] {
            let managed = EventAttachment {
                url: Some(url.to_string()),
                managed_id: Some("97S".to_string()),
                ..Default::default()
            };
            assert!(managed.graph_id().is_none(), "{url} is a CalDAV attachment");
        }
    }
}
