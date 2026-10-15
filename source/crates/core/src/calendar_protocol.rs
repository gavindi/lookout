//! The backend-neutral actor contract for a calendar sync session:
//! [`CalendarCommand`] in, [`CalendarSessionEvent`] out, plus the
//! [`ConnectionState`] lifecycle both ends report.
//!
//! It lives in `lookout-core` - the crate every backend already depends on -
//! so the CalDAV (`lookout-dav`) and Microsoft Graph (`lookout-graph`) session
//! actors can share one vocabulary without either depending on the other, and
//! the app's calendar UI can drive either backend unchanged
//! (OUTLOOK_PARITY.md §5.1, item 5.1; §2.4 decision 1).
//!
//! These are pure data types: no I/O, no async channels. The actors own the
//! `async_channel` sender/receiver ends; this module only names what crosses
//! them.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, NaiveDate, Utc};

use crate::calendar::{CalendarEvent, CalendarInfo, CalendarTask, EventOccurrence, SchedulingInfo};
use crate::email::EmailAddress;
use crate::freebusy::AttendeeAvailability;
use crate::ids::{CalendarId, EventUid};

#[derive(Debug, Clone)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Idle,
    Busy,
    Error { message: String, retryable: bool },
}

/// A response to a meeting invitation, as Graph's RSVP actions model it
/// (item 12 / CAL-15). Used by [`CalendarCommand::RespondInvitation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvitationResponse {
    Accept,
    Tentative,
    Decline,
}

/// A proposed new time for an invitation (iTIP COUNTER, item 12 / CAL-17):
/// the start/end the attendee would prefer, as UTC instants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProposedTime {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

#[derive(Debug)]
pub enum CalendarCommand {
    /// Any date within the month to display; the actor resolves the
    /// month's own first/last-instant bounds itself.
    SyncMonth(NaiveDate),
    /// Fetch one month's occurrences without making it the polled month -
    /// the Lookout dashboard's "upcoming events" horizon reaches into next
    /// month without hijacking `current_month`, so the 5-minute poll keeps
    /// re-syncing whatever month the calendar view is showing.
    FetchMonth(NaiveDate),
    /// Force a resync of the currently-displayed month outside the poll cadence.
    Refresh,
    /// A hint that it's worth retrying the connection now rather than
    /// waiting out the current backoff delay. A no-op if already connected.
    Reconnect,
    /// Store a brand-new event: PUT the serialized VEVENT as a new calendar
    /// object under `event.calendar_id`'s collection (a client-generated
    /// `<uid>.ics` href, `If-None-Match: *`). Resyncs the on-screen month on
    /// success so the new occurrence renders.
    CreateEvent {
        event: Box<CalendarEvent>,
    },
    /// Store an edited event in place: PUT to `event.href` with `event.etag`
    /// as `If-Match` (fails with a surfaced error rather than clobbering a
    /// concurrent change if the etag is stale). Resyncs on success.
    UpdateEvent {
        event: Box<CalendarEvent>,
    },
    /// Delete the calendar object at `href` (with an optional `etag` as
    /// `If-Match`). Resyncs on success.
    DeleteEvent {
        calendar_id: CalendarId,
        href: String,
        etag: Option<String>,
    },
    /// Respond to a meeting invitation stored in a Graph calendar (item 12):
    /// Graph delivers iTIP itself, so the app calls the server's own RSVP
    /// action (`/me/events/{id}/accept|tentativelyAccept|decline`) rather than
    /// sending an iMIP REPLY. `event_id` is the event's Graph id
    /// (`EventOccurrence::href`); `proposed_new_time` carries an iTIP COUNTER.
    /// CalDAV accounts never receive this - their RSVP is an iMIP REPLY sent by
    /// the app.
    RespondInvitation {
        calendar_id: CalendarId,
        event_id: String,
        response: InvitationResponse,
        comment: Option<String>,
        send_response: bool,
        proposed_new_time: Option<ProposedTime>,
    },
    /// Force a resync of every task in every calendar, outside the poll
    /// cadence (e.g. right after a task edit lands).
    SyncTasks,
    /// Ask the server for attendees' free/busy over `[start, end)` by POSTing
    /// a `VFREEBUSY` request to the account's schedule-outbox (RFC 6638 §5,
    /// CAL-19). `request_id` is echoed back on the `FreeBusyUpdated`/
    /// `FreeBusyFailed` event so the caller can drop a stale answer. A no-op
    /// failure when the account has no outbox.
    FetchFreeBusy {
        organizer: EmailAddress,
        attendees: Vec<EmailAddress>,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        request_id: u64,
    },
    /// Store a brand-new task: PUT the serialized VTODO as a new calendar
    /// object under `task.calendar_id`'s collection (a client-generated
    /// `<uid>.ics` href, `If-None-Match: *`). Resyncs tasks on success.
    CreateTask {
        task: Box<CalendarTask>,
    },
    /// Store an edited task in place: PUT to `task.href` with `task.etag`
    /// as `If-Match`. Resyncs tasks on success.
    UpdateTask {
        task: Box<CalendarTask>,
    },
    /// Delete the calendar object at `href` (with an optional `etag` as
    /// `If-Match`). Resyncs tasks on success.
    DeleteTask {
        calendar_id: CalendarId,
        href: String,
        etag: Option<String>,
    },
    Shutdown,
}

/// Named `CalendarSessionEvent` rather than `CalendarEvent` to avoid
/// colliding with `crate::CalendarEvent` when both are in scope unqualified
/// (e.g. in `crates/app/src/window.rs`).
#[derive(Debug)]
pub enum CalendarSessionEvent {
    ConnectionStateChanged(ConnectionState),
    CalendarsUpdated(Vec<CalendarInfo>),
    /// The account principal's RFC 6638 scheduling capability
    /// (`schedule-outbox-URL`/`schedule-inbox-URL`), reported once per
    /// connection right after discovery. The app stores it per account: an
    /// outbox means the server delivers iTIP itself, so the client-side iMIP
    /// sender stands down (and a later CAL-19 free/busy request has its POST
    /// target).
    SchedulingUpdated(SchedulingInfo),
    OccurrencesUpdated {
        month: NaiveDate,
        occurrences: Vec<EventOccurrence>,
    },
    TasksUpdated(Vec<CalendarTask>),
    /// A `FetchFreeBusy` answer: one availability per requested attendee, in
    /// request order (CAL-19). `request_id` matches the originating command.
    FreeBusyUpdated {
        request_id: u64,
        availabilities: Vec<AttendeeAvailability>,
    },
    /// A `FetchFreeBusy` request failed, or the account has no schedule-outbox
    /// to ask (in which case the app falls back to the user's own calendars).
    FreeBusyFailed {
        request_id: u64,
        message: String,
    },
    Error(String),
    /// A `CreateEvent`/`UpdateEvent` request failed. Kept distinct from
    /// `Error` so the UI can roll back exactly the occurrence it
    /// optimistically moved (if any - harmless no-op otherwise), mirroring
    /// `lookout_mail::session::AccountEvent::MoveFailed`.
    EventSaveFailed {
        uid: EventUid,
        recurrence_id: Option<DateTime<Utc>>,
        message: String,
    },
}
