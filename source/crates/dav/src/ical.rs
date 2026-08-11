use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use icalendar::{
    Alarm, Attendee as IcalAttendee, Calendar, CalendarComponent, CalendarDateTime, Class, Component, DatePerhapsTime, EventLike, PartStat, Property, Role, TodoStatus, Trigger,
};
use lookout_core::{
    Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, CalendarId, CalendarTask, EmailAddress, EventSensitivity, EventTransparency, EventUid, ImipInvitation, ImipMethod,
    RecurrenceRange, TaskPriority, TaskStatus, TaskUid,
};

/// Parses every VEVENT out of a raw iCalendar document (as returned by a
/// CalDAV `calendar-data` property) into [`CalendarEvent`]s, attaching no
/// href/etag metadata (those live in the enclosing `multistatus` response,
/// not the `calendar-data`). Prefer [`parse_vevents_with_meta`] when the
/// caller has the response's `<href>`/`getetag` at hand.
pub fn parse_vevents(calendar_id: &CalendarId, ics: &str) -> Vec<CalendarEvent> {
    parse_vevents_with_meta(calendar_id, ics, None, None)
}

/// Like [`parse_vevents`], but stamps every parsed event with the given
/// resource href/etag - the CalDAV `multistatus` `<href>` and `<getetag>`
/// that accompany a `calendar-data`, which the write path needs to PUT/DELETE
/// the resource back.
///
/// A VEVENT with `RECURRENCE-ID` (a per-occurrence override of one instance
/// of a recurring series) is parsed as a standalone event with its
/// `recurrence_id` set - merging it into its master's expansion is the
/// expander's job (`expand_occurrences`), not the parser's.
///
/// Known, accepted simplifications for this pass:
/// - `TZID`-qualified `DTSTART`/`DTEND` are resolved to UTC via
///   `CalendarDateTime::try_into_utc` (icalendar's own `chrono-tz`
///   integration, enabled via this crate's `chrono-tz` feature). A
///   "floating" (no `Z`, no `TZID`) datetime - which icalendar's own docs
///   call "a red flag" - falls back to being treated as UTC, with a
///   warning, rather than silently dropping the event.
pub fn parse_vevents_with_meta(calendar_id: &CalendarId, ics: &str, href: Option<&str>, etag: Option<&str>) -> Vec<CalendarEvent> {
    let calendar: Calendar = match ics.parse() {
        Ok(cal) => cal,
        Err(e) => {
            tracing::warn!("failed to parse iCalendar data: {e}");
            return Vec::new();
        }
    };

    calendar
        .components
        .iter()
        .filter_map(|c| match c {
            CalendarComponent::Event(event) => convert_event(calendar_id, event),
            _ => None,
        })
        .map(|mut event| {
            event.href = href.map(str::to_string);
            event.etag = etag.map(str::to_string);
            event
        })
        .collect()
}

fn convert_event(calendar_id: &CalendarId, event: &icalendar::Event) -> Option<CalendarEvent> {
    let uid = event.get_uid()?.to_string();
    let start = event.get_start()?;
    let (start_utc, all_day) = to_utc(&start)?;

    let end_utc = if let Some(end) = event.get_end() {
        to_utc(&end)?.0
    } else if let Some(duration_str) = event.property_value("DURATION") {
        start_utc + parse_ical_duration(duration_str)
    } else if all_day {
        // RFC 5545 §3.6.1: no DTEND/DURATION on an all-day DTSTART means a
        // single-day span.
        start_utc + Duration::days(1)
    } else {
        // ...and no DTEND/DURATION on a DATE-TIME DTSTART means a
        // zero-length event.
        start_utc
    };

    Some(CalendarEvent {
        uid: EventUid(uid),
        calendar_id: calendar_id.clone(),
        summary: event.get_summary().map(|s| s.to_string()),
        description: event.get_description().map(|s| s.to_string()),
        location: event.get_location().map(|s| s.to_string()),
        start: start_utc,
        end: end_utc,
        all_day,
        rrule: event.property_value("RRULE").map(|s| s.to_string()),
        recurrence_id: parse_recurrence_id(event),
        recurrence_range: parse_recurrence_range(event),
        exdates: parse_datetime_list(event, "EXDATE"),
        rdates: parse_datetime_list(event, "RDATE"),
        // The parse side has no href/etag (those come from the enclosing
        // `multistatus` `<href>`/`getetag`, not the `calendar-data`) - the
        // caller fills them in after the fact via
        // [`parse_vevents_with_meta`].
        href: None,
        etag: None,
        attendees: parse_attendees(event),
        organizer: parse_organizer(event),
        categories: parse_categories(event),
        sensitivity: event.get_class().map(from_ical_class).unwrap_or_default(),
        transparency: match event.property_value("TRANSP") {
            Some("TRANSPARENT") => EventTransparency::Free,
            _ => EventTransparency::Busy,
        },
        conference_url: event.property_value("CONFERENCE").or_else(|| event.get_url()).map(str::to_string),
        reminder_minutes_before: parse_reminder(event),
    })
}

/// Every parsed `Property` under `key`, in multi-property order then
/// single-property order. `EXDATE`/`RDATE` are legally repeatable and land
/// in `multi_properties` when repeated but `properties` when they appear
/// once; `RECURRENCE-ID` appears exactly once and normally sits in
/// `properties` - checking both maps keeps the lookups uniform.
fn property_refs<'a>(c: &'a impl Component, key: &str) -> Vec<&'a Property> {
    let mut refs: Vec<&'a Property> = c.multi_properties().get(key).map(|props| props.iter().collect()).unwrap_or_default();
    if let Some(property) = c.properties().get(key) {
        refs.push(property);
    }
    refs
}

/// The `RECURRENCE-ID` of a per-occurrence override, resolved to UTC the
/// same way `DTSTART` is (TZID-qualified datetimes via `chrono-tz`; DATE
/// values - all-day series - pinned to midnight UTC).
fn parse_recurrence_id(event: &icalendar::Event) -> Option<DateTime<Utc>> {
    let property = property_refs(event, "RECURRENCE-ID").into_iter().next()?;
    let dpt = DatePerhapsTime::from_property(property)?;
    to_utc(&dpt).map(|(utc, _)| utc)
}

/// The `RANGE` parameter of the override's `RECURRENCE-ID` (RFC 5545
/// §3.2.13): only `THISANDFUTURE` is a real value; anything else (including
/// an absent parameter) is a single-instance override.
fn parse_recurrence_range(event: &icalendar::Event) -> RecurrenceRange {
    let range = property_refs(event, "RECURRENCE-ID")
        .first()
        .and_then(|p| p.params().get("RANGE"))
        .map(|p| p.value().to_ascii_uppercase());
    if range.as_deref() == Some("THISANDFUTURE") {
        RecurrenceRange::ThisAndFuture
    } else {
        RecurrenceRange::This
    }
}

/// `EXDATE`/`RDATE` values as UTC datetimes, resolved the same way
/// `DTSTART` is (see [`to_utc`]) so TZID-qualified and all-day series
/// exclusions round-trip losslessly.
fn parse_datetime_list(event: &icalendar::Event, key: &str) -> Vec<DateTime<Utc>> {
    property_refs(event, key)
        .iter()
        .filter_map(|p| DatePerhapsTime::from_property(p))
        .filter_map(|dpt| to_utc(&dpt).map(|(utc, _)| utc))
        .collect()
}

/// Strips a `mailto:`/`MAILTO:` scheme prefix, if present, from a CAL-ADDRESS.
fn strip_mailto(cal_address: &str) -> &str {
    cal_address.strip_prefix("mailto:").or_else(|| cal_address.strip_prefix("MAILTO:")).unwrap_or(cal_address)
}

fn parse_attendees(event: &icalendar::Event) -> Vec<Attendee> {
    event
        .get_attendees()
        .into_iter()
        .map(|a| Attendee {
            address: EmailAddress {
                name: a.cn.clone(),
                address: strip_mailto(&a.cal_address).to_string(),
            },
            role: match a.role {
                Some(Role::OptParticipant) => AttendeeRole::Optional,
                _ => AttendeeRole::Required,
            },
            status: match a.part_stat {
                Some(PartStat::Accepted) => AttendeeStatus::Accepted,
                Some(PartStat::Declined) => AttendeeStatus::Declined,
                Some(PartStat::Tentative) => AttendeeStatus::Tentative,
                _ => AttendeeStatus::NeedsAction,
            },
        })
        .collect()
}

/// `ORGANIZER` isn't given a typed accessor by `icalendar` - read via the
/// same generic `property_value` mechanism used for `RRULE`. The `CN`
/// parameter (display name) isn't exposed through `property_value`, so the
/// parsed organizer only ever carries a bare address.
fn parse_organizer(event: &icalendar::Event) -> Option<EmailAddress> {
    event.property_value("ORGANIZER").map(|v| EmailAddress::new(strip_mailto(v).to_string()))
}

/// `CATEGORIES` is one of `icalendar`'s hard-coded multi-value properties
/// (RFC 5545 §3.8.1.2 allows it to repeat), so it lives in
/// `multi_properties`, not the single-valued `properties()` map -
/// `property_value("CATEGORIES")` always returns `None`. Each occurrence's
/// value may itself be a comma-separated list, so every one is split.
/// Generic over `Component` because both `VEVENT` and `VTODO` carry it.
fn parse_categories(c: &impl Component) -> Vec<String> {
    c.multi_properties()
        .get("CATEGORIES")
        .map(|props| {
            props
                .iter()
                .flat_map(|p| p.value().split(','))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn from_ical_class(class: Class) -> EventSensitivity {
    match class {
        Class::Public => EventSensitivity::Public,
        Class::Private => EventSensitivity::Private,
        Class::Confidential => EventSensitivity::Confidential,
    }
}

fn to_ical_class(sensitivity: EventSensitivity) -> Class {
    match sensitivity {
        EventSensitivity::Public => Class::Public,
        EventSensitivity::Private => Class::Private,
        EventSensitivity::Confidential => Class::Confidential,
    }
}

/// Reads the first `VALARM` child's `TRIGGER` as "minutes before start".
/// Multiple/repeating alarms and absolute-time or after-start/end triggers
/// aren't modeled - only a single simple "N minutes before" alarm.
///
/// Parses the raw duration value directly rather than going through
/// `icalendar::Trigger::try_from` - that round-trips `chrono::Duration`'s own
/// `Display` output (e.g. `"-PT900S"`) through the `iso8601` crate, which
/// doesn't accept a leading sign and always fails on exactly the negative
/// ("before start") durations a reminder needs (verified against
/// `icalendar` 0.17.13 with a standalone repro). Reusing
/// [`parse_ical_duration`] on the unsigned remainder sidesteps that bug.
fn parse_reminder(event: &icalendar::Event) -> Option<i64> {
    let alarm = event.components().iter().find(|c| c.component_kind() == "VALARM")?;
    let raw = alarm.property_value("TRIGGER")?;
    let unsigned = raw.strip_prefix('-')?;
    Some(parse_ical_duration(unsigned).num_minutes())
}

fn to_utc(dpt: &DatePerhapsTime) -> Option<(DateTime<Utc>, bool)> {
    match dpt {
        DatePerhapsTime::Date(date) => Some((Utc.from_utc_datetime(&date.and_time(NaiveTime::MIN)), true)),
        DatePerhapsTime::DateTime(cdt) => {
            let utc = resolve_datetime_utc(cdt).or_else(|| match cdt {
                CalendarDateTime::Floating(naive) => {
                    tracing::warn!("event has a floating (no timezone) date-time; treating as UTC");
                    Some(Utc.from_utc_datetime(naive))
                }
                _ => None,
            })?;
            Some((utc, false))
        }
    }
}

/// Resolves a `DTSTART`/`DTEND`/`RECURRENCE-ID`/`EXDATE`/`RDATE` date-time to
/// its UTC instant. IANA `TZID`s resolve through chrono-tz directly; Windows
/// timezone IDs (Outlook/Exchange iMIP invitations - Teams meetings included
/// - stamp their events with `TZID=W. Europe Standard Time` and friends) are
/// looked up in the CLDR mapping first, since chrono-tz only knows IANA
/// names. A `TZID` neither set understands is dropped with a warning rather
/// than guessed - the caller then discards the event the same way it does
/// any other unresolvable one.
fn resolve_datetime_utc(cdt: &CalendarDateTime) -> Option<DateTime<Utc>> {
    match cdt {
        CalendarDateTime::Utc(inner) => Some(*inner),
        CalendarDateTime::Floating(_) => None,
        CalendarDateTime::WithTimezone { date_time, tzid } => {
            let zone = std::str::FromStr::from_str(tzid)
                .ok()
                .or_else(|| crate::tzmap::windows_to_iana(tzid).and_then(|iana| iana.parse().ok()));
            match zone.and_then(|tz: chrono_tz::Tz| tz.from_local_datetime(date_time).single()) {
                Some(dt) => Some(dt.with_timezone(&Utc)),
                None => {
                    tracing::warn!("event references unknown timezone {tzid:?}; dropping it");
                    None
                }
            }
        }
    }
}

/// Parses an RFC 5545 `DURATION` value (e.g. `"PT1H30M"`) into a
/// [`chrono::Duration`]. RFC 5545's `DURATION` value type never actually
/// carries year/month components (only week, or day+time) - but the
/// underlying `iso8601` parser accepts the fuller ISO 8601 grammar, so a
/// year/month component (if a non-conformant server ever emits one) is
/// approximated as 365/30 days respectively rather than rejected outright.
fn parse_ical_duration(raw: &str) -> Duration {
    match iso8601::duration(raw) {
        Ok(iso8601::Duration::Weeks(w)) => Duration::weeks(w as i64),
        Ok(iso8601::Duration::YMDHMS {
            year,
            month,
            day,
            hour,
            minute,
            second,
            millisecond,
        }) => {
            Duration::days(i64::from(year) * 365 + i64::from(month) * 30 + i64::from(day))
                + Duration::hours(i64::from(hour))
                + Duration::minutes(i64::from(minute))
                + Duration::seconds(i64::from(second))
                + Duration::milliseconds(i64::from(millisecond))
        }
        Err(e) => {
            tracing::warn!("failed to parse DURATION {raw:?}: {e}; treating event as zero-length");
            Duration::zero()
        }
    }
}

/// Serializes a [`CalendarEvent`] into a single-VEVENT iCalendar document
/// suitable for a CalDAV `PUT` (the create/update body). Round-trips through
/// [`parse_vevents`]: UID/DTSTART/DTEND are preserved, all-day events become
/// `VALUE=DATE` values and timed events UTC `DATE-TIME`, and an existing RRULE
/// is carried over verbatim so editing a recurring master edits the whole
/// series rather than flattening it into a one-off (per-occurrence overrides
/// remain out of scope, see the parse-side note on [`parse_vevents_with_meta`]).
///
/// The `icalendar` builder emits RFC 5545-correct CRLF line folding and
/// escaping for us; `DTSTAMP` is generated fresh on every build (the editor
/// intentionally never edits it).
pub fn build_vcalendar(event: &CalendarEvent) -> String {
    let mut calendar = icalendar::Calendar::new();
    calendar.push(build_vevent(event).done());
    format!("{calendar}")
}

/// Serializes a [`CalendarEvent`] into an iMIP message payload (RFC 6047):
/// the same single-VEVENT document [`build_vcalendar`] produces, plus the
/// calendar-level `METHOD` property that tells the recipient how to treat it
/// (`REQUEST` an invitation, `REPLY` an attendee's RSVP, `CANCEL` a
/// withdrawal). `METHOD` is *never* emitted by [`build_vcalendar`] - a CalDAV
/// `PUT` body must not carry it - which is why this is a separate entry point
/// rather than a parameter on the shared builder.
pub fn build_imip_vcalendar(event: &CalendarEvent, method: ImipMethod) -> String {
    let mut calendar = icalendar::Calendar::new();
    let method = match method {
        ImipMethod::Request => "REQUEST",
        ImipMethod::Reply => "REPLY",
        ImipMethod::Cancel => "CANCEL",
    };
    calendar.append_property(icalendar::Property::new("METHOD", method));
    calendar.push(build_vevent(event).done());
    format!("{calendar}")
}

/// Parses every VTODO out of a raw iCalendar document into [`CalendarTask`]s,
/// with no href/etag metadata (those live in the enclosing `multistatus`
/// response). Prefer [`parse_vtodos_with_meta`] when the caller has the
/// response's `<href>`/`getetag` at hand. VEVENTs (and any other component)
/// in the same document are ignored - one CalDAV resource holds one task.
///
/// Known, accepted simplifications for this pass:
/// - A `DATE`-valued `DUE` (an all-day-style task) normalizes to UTC
///   midnight; the all-day-ness is not modeled, so a round-trip writes the
///   due time back as a UTC `DATE-TIME`. Tasks are typically timed, and this
///   mirrors how the event side treats `VALUE=DATE` starts elsewhere.
/// - `DURATION`-based tasks (no `DUE`) get their due time computed from
///   `DTSTART` + `DURATION`; a `DURATION` with no `DTSTART` (relative to the
///   creation date per RFC 5545 §3.6.2) can't be anchored and is dropped.
/// - Task `RRULE` (recurrence) is not modeled - see [`CalendarTask`].
pub fn parse_vtodos(calendar_id: &CalendarId, ics: &str) -> Vec<CalendarTask> {
    parse_vtodos_with_meta(calendar_id, ics, None, None)
}

/// Like [`parse_vtodos`], but stamps every parsed task with the resource
/// href/etag from the enclosing CalDAV `multistatus` response, which the
/// write path needs to PUT/DELETE the resource back.
pub fn parse_vtodos_with_meta(calendar_id: &CalendarId, ics: &str, href: Option<&str>, etag: Option<&str>) -> Vec<CalendarTask> {
    let calendar: Calendar = match ics.parse() {
        Ok(cal) => cal,
        Err(e) => {
            tracing::warn!("failed to parse iCalendar data: {e}");
            return Vec::new();
        }
    };

    calendar
        .components
        .iter()
        .filter_map(|c| match c {
            CalendarComponent::Todo(todo) => convert_todo(calendar_id, todo),
            _ => None,
        })
        .map(|mut task| {
            task.href = href.map(str::to_string);
            task.etag = etag.map(str::to_string);
            task
        })
        .collect()
}

fn convert_todo(calendar_id: &CalendarId, todo: &icalendar::Todo) -> Option<CalendarTask> {
    let uid = todo.get_uid()?.to_string();

    // `DUE` is the task's only unambiguous temporal anchor. A
    // `DURATION`-only task is anchored relative to `DTSTART` when that's
    // present (the RFC's effective-duration reading); a floating `DURATION`
    // has no computable due time, so the task keeps just its start.
    let start = todo.get_start().and_then(|dpt| to_utc(&dpt).map(|(utc, _)| utc));
    let due = todo.get_due().and_then(|dpt| to_utc(&dpt).map(|(utc, _)| utc)).or_else(|| {
        let duration = todo.property_value("DURATION").map(parse_ical_duration)?;
        Some(start? + duration)
    });

    Some(CalendarTask {
        uid: TaskUid(uid),
        calendar_id: calendar_id.clone(),
        summary: todo.get_summary().map(str::to_string),
        description: todo.get_description().map(str::to_string),
        due,
        start,
        completed: todo.get_completed(),
        status: match todo.get_status() {
            Some(TodoStatus::NeedsAction) | None => TaskStatus::NeedsAction,
            Some(TodoStatus::InProcess) => TaskStatus::InProgress,
            Some(TodoStatus::Completed) => TaskStatus::Completed,
            Some(TodoStatus::Cancelled) => TaskStatus::Cancelled,
        },
        priority: TaskPriority(todo.property_value("PRIORITY").and_then(|p| p.parse().ok()).unwrap_or(0)),
        percent_complete: todo.get_percent_complete(),
        categories: parse_categories(todo),
        // The parse side has no href/etag (those come from the enclosing
        // `multistatus` `<href>`/`getetag`, not the `calendar-data`) - the
        // caller fills them in after the fact via
        // [`parse_vtodos_with_meta`].
        href: None,
        etag: None,
    })
}

/// Serializes a [`CalendarTask`] into a single-VTODO iCalendar document
/// suitable for a CalDAV `PUT` (the create/update body). Round-trips through
/// [`parse_vtodos`]: UID/SUMMARY/DESCRIPTION/DUE/STATUS/PRIORITY/
/// PERCENT-COMPLETE/COMPLETED are preserved; a task with no due time writes
/// no `DUE` line. The `icalendar` builder emits RFC 5545-correct CRLF line
/// folding and escaping for us; `DTSTAMP` is generated fresh on every build.
pub fn build_vtodo_calendar(task: &CalendarTask) -> String {
    let mut calendar = icalendar::Calendar::new();
    calendar.push(build_vtodo(task).done());
    format!("{calendar}")
}

fn build_vtodo(task: &CalendarTask) -> icalendar::Todo {
    let mut vtodo = icalendar::Todo::new();

    vtodo.uid(&task.uid.0);
    if let Some(summary) = &task.summary {
        vtodo.summary(summary);
    }
    if let Some(description) = &task.description {
        vtodo.description(description);
    }
    // RFC 5545 §3.6.2 forbids `DUE` and `DURATION` together; the model only
    // carries a due time, so a `DURATION`-based task that was parsed with a
    // computed due writes it back as an explicit `DUE` - equivalent, since
    // `DTSTART` is written too.
    if let Some(due) = task.due {
        vtodo.due(due);
    }
    if let Some(start) = task.start {
        vtodo.starts(start);
    }
    match task.status {
        TaskStatus::NeedsAction => {}
        TaskStatus::InProgress => {
            vtodo.status(TodoStatus::InProcess);
        }
        TaskStatus::Completed => {
            vtodo.status(TodoStatus::Completed);
        }
        TaskStatus::Cancelled => {
            vtodo.status(TodoStatus::Cancelled);
        }
    }
    if let Some(completed) = task.completed {
        vtodo.completed(completed);
    }
    if task.priority.0 != 0 {
        vtodo.priority(u32::from(task.priority.0));
    }
    if let Some(percent) = task.percent_complete {
        vtodo.percent_complete(percent);
    }
    if !task.categories.is_empty() {
        vtodo.append_multi_property(Property::new("CATEGORIES", task.categories.join(",")));
    }

    vtodo
}

/// Parses a message's `text/calendar` payload into the [`ImipInvitation`] the
/// reading pane's banner acts on. Returns `None` when the document carries no
/// parseable VEVENT (a `text/calendar` part that is somehow not an event -
/// e.g. a bare VTODO or a VALARM-only document - is not an invitation).
/// The method comes from the raw document via [`lookout_core::parse_imip_method`];
/// the event's summary, organizer and the details the reading pane's invite
/// card shows (start/end/all-day/location/description/recurrence) come from
/// the parsed VEVENT.
pub fn parse_imip_invitation(ics: &str) -> Option<ImipInvitation> {
    let method = lookout_core::parse_imip_method(ics);
    // The CalendarId is purely the parsed event's ownership stamp and never
    // escapes this function - the app re-stamps the event onto the calendar
    // it saves into.
    let events = parse_vevents(&CalendarId("iMIP".to_string()), ics);
    let event = events.into_iter().next()?;
    Some(ImipInvitation {
        method,
        ics: ics.to_string(),
        summary: event.summary,
        organizer: event.organizer,
        in_reply_to: None,
        start: event.start,
        end: event.end,
        all_day: event.all_day,
        location: event.location,
        description: event.description,
        rrule: event.rrule,
    })
}

fn build_vevent(event: &CalendarEvent) -> icalendar::Event {
    let mut vevent = icalendar::Event::new();

    vevent.uid(&event.uid.0);
    if let Some(summary) = &event.summary {
        vevent.summary(summary);
    }
    if let Some(description) = &event.description {
        vevent.description(description);
    }
    if let Some(location) = &event.location {
        vevent.location(location);
    }
    if event.all_day {
        // RFC 5545 §3.6.1: all-day DTSTART/DTEND are DATE values, and DTEND is
        // exclusive (a one-day event is `DTSTART;VALUE=DATE:...` /
        // `DTEND;VALUE=DATE:...+1`), matching what `convert_event`'s parser
        // reconstructs (`end - start == 1 day` round-trips exactly).
        vevent.all_day(event.start.date_naive());
        vevent.ends(event.end.date_naive());
    } else {
        vevent.starts(event.start);
        vevent.ends(event.end);
    }
    if let Some(rrule) = &event.rrule {
        // `EventLike::recurrence` would need a parsed RRule; we already hold
        // the raw RFC 5545 `RECUR` string the master was stored with, so put
        // it back verbatim to avoid a parse/re-serialize round trip.
        vevent.append_property(icalendar::Property::new("RRULE", rrule.as_str()));
    }
    // A per-occurrence override is a separate VEVENT sharing the master's
    // UID, anchored by RECURRENCE-ID. All-day series anchor on DATE values
    // (matching the master's DTSTART), timed series on UTC DATE-TIME. The
    // RANGE=THISANDFUTURE parameter (RFC 5545 §3.2.13) is emitted when the
    // override extends over every later instance.
    if let Some(recurrence_id) = &event.recurrence_id {
        let mut prop = recurrence_property("RECURRENCE-ID", *recurrence_id, event.all_day);
        if event.recurrence_range == RecurrenceRange::ThisAndFuture {
            prop.add_parameter("RANGE", "THISANDFUTURE");
        }
        vevent.append_property(prop.done());
    }
    // EXDATEs/RDATEs repeat, so each goes through `append_multi_property`
    // (the parser reassembles them from either map - see `property_refs`).
    // Timestamps are re-emitted in UTC, which is exactly what the parser
    // produced; a TZID-qualified exclusion a server stored is normalized to
    // its UTC instant but keeps its exact meaning.
    for exdate in &event.exdates {
        vevent.append_multi_property(recurrence_property("EXDATE", *exdate, event.all_day).done());
    }
    for rdate in &event.rdates {
        vevent.append_multi_property(recurrence_property("RDATE", *rdate, event.all_day).done());
    }
    append_attendees(&mut vevent, &event.attendees);
    // RFC 5545 §3.6.1 requires an ORGANIZER whenever ATTENDEEs are present;
    // omitted otherwise, and omitted even with attendees if the owning
    // account's own address couldn't be determined (see `CalendarEvent::organizer`).
    if !event.attendees.is_empty() {
        if let Some(organizer) = &event.organizer {
            let mut prop = Property::new("ORGANIZER", format!("mailto:{}", organizer.address));
            if let Some(name) = &organizer.name {
                prop.add_parameter("CN", name);
            }
            vevent.append_property(prop.done());
        }
    }
    if !event.categories.is_empty() {
        vevent.append_property(Property::new("CATEGORIES", event.categories.join(",")));
    }
    // `Class::Public` is the RFC default - omit it so a plain event's
    // serialized ICS doesn't grow noise it didn't have before this field
    // existed.
    if event.sensitivity != EventSensitivity::Public {
        vevent.class(to_ical_class(event.sensitivity));
    }
    if event.transparency == EventTransparency::Free {
        vevent.append_property(Property::new("TRANSP", "TRANSPARENT"));
    }
    if let Some(conference_url) = &event.conference_url {
        vevent.append_property(Property::new("CONFERENCE", conference_url.as_str()));
    }
    if let Some(minutes) = event.reminder_minutes_before {
        vevent.alarm(Alarm::display("Reminder", Trigger::before_start(Duration::minutes(minutes))));
    }

    vevent
}

/// A `RECURRENCE-ID`/`EXDATE`/`RDATE` property for `dt`, emitted as a DATE
/// value for all-day series (anchored on midnight UTC, the form
/// [`convert_event`]'s parser produces) or a UTC DATE-TIME otherwise.
fn recurrence_property(key: &str, dt: DateTime<Utc>, all_day: bool) -> Property {
    let value = if all_day {
        dt.format("%Y%m%d").to_string()
    } else {
        dt.format("%Y%m%dT%H%M%SZ").to_string()
    };
    let mut prop = Property::new(key, value);
    if all_day {
        prop.add_parameter("VALUE", "DATE");
    }
    prop
}

fn append_attendees(vevent: &mut icalendar::Event, attendees: &[Attendee]) {
    for attendee in attendees {
        let mut ical_attendee = IcalAttendee::new(format!("mailto:{}", attendee.address.address))
            .role(match attendee.role {
                AttendeeRole::Required => Role::ReqParticipant,
                AttendeeRole::Optional => Role::OptParticipant,
            })
            .partstat(match attendee.status {
                AttendeeStatus::NeedsAction => PartStat::NeedsAction,
                AttendeeStatus::Accepted => PartStat::Accepted,
                AttendeeStatus::Declined => PartStat::Declined,
                AttendeeStatus::Tentative => PartStat::Tentative,
            });
        if let Some(name) = &attendee.address.name {
            ical_attendee = ical_attendee.cn(name.clone());
        }
        vevent.attendee(ical_attendee);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::parse_imip_method;

    fn cal_id() -> CalendarId {
        CalendarId("test-account:test-calendar".to_string())
    }

    #[test]
    fn parses_plain_datetime_event() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:plain-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Plain meeting\r\nLOCATION:Room 1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid.0, "plain-1@example.com");
        assert_eq!(events[0].summary.as_deref(), Some("Plain meeting"));
        assert_eq!(events[0].location.as_deref(), Some("Room 1"));
        assert!(!events[0].all_day);
        assert_eq!(events[0].start, "2026-07-15T14:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(events[0].end, "2026-07-15T15:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert!(events[0].rrule.is_none());
    }

    #[test]
    fn parses_recurring_event_rrule_string() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:weekly-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260706T090000Z\r\nDTEND:20260706T093000Z\r\nSUMMARY:Weekly standup\r\nRRULE:FREQ=WEEKLY;COUNT=10\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].rrule.as_deref(), Some("FREQ=WEEKLY;COUNT=10"));
    }

    #[test]
    fn parses_all_day_event_as_one_day_span() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:allday-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART;VALUE=DATE:20260720\r\nDTEND;VALUE=DATE:20260721\r\nSUMMARY:Conference day\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert!(events[0].all_day);
        assert_eq!(events[0].start, "2026-07-20T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(events[0].end, "2026-07-21T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
    }

    #[test]
    fn resolves_tzid_qualified_datetime_to_utc() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VTIMEZONE\r\nTZID:America/New_York\r\nBEGIN:STANDARD\r\nDTSTART:19701101T020000\r\nTZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:19700308T020000\r\nTZOFFSETFROM:-0500\r\nTZOFFSETTO:-0400\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:tzid-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART;TZID=America/New_York:20260710T090000\r\nDTEND;TZID=America/New_York:20260710T100000\r\nSUMMARY:NY morning call\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        // 2026-07-10 is in EDT (UTC-4), so 09:00 local is 13:00 UTC.
        assert_eq!(events[0].start, "2026-07-10T13:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(events[0].end, "2026-07-10T14:00:00Z".parse::<DateTime<Utc>>().unwrap());
    }

    #[test]
    fn falls_back_to_zero_length_when_no_dtend_or_duration() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:no-end@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nSUMMARY:Point in time\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start, events[0].end);
    }

    #[test]
    fn uses_duration_property_when_no_dtend() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:dur-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDURATION:PT1H30M\r\nSUMMARY:Duration-based\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].end - events[0].start, Duration::minutes(90));
    }

    #[test]
    fn malformed_calendar_data_yields_no_events_rather_than_panicking() {
        let events = parse_vevents(&cal_id(), "this is not iCalendar data at all");
        assert!(events.is_empty());
    }

    fn full_event() -> CalendarEvent {
        CalendarEvent {
            uid: EventUid("evt-2@example.com".to_string()),
            calendar_id: cal_id(),
            summary: Some("Team sync".to_string()),
            description: Some("Agenda & notes <html>".to_string()),
            location: Some("Room 3".to_string()),
            start: "2026-07-15T14:00:00Z".parse().unwrap(),
            end: "2026-07-15T15:00:00Z".parse().unwrap(),
            all_day: false,
            rrule: None,
            recurrence_id: None,
            recurrence_range: RecurrenceRange::default(),
            exdates: Vec::new(),
            rdates: Vec::new(),
            href: None,
            etag: None,
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: EventSensitivity::default(),
            transparency: EventTransparency::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    #[test]
    fn build_vcalendar_round_trips_a_timed_event() {
        let ics = build_vcalendar(&full_event());
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"), "should be CRLF line endings, got: {ics}");
        let events = parse_vevents(&cal_id(), &ics);
        assert_eq!(events.len(), 1);
        let round = &events[0];
        assert_eq!(round.uid, full_event().uid);
        assert_eq!(round.summary, full_event().summary);
        assert_eq!(round.description, full_event().description);
        assert_eq!(round.location, full_event().location);
        assert_eq!(round.start, full_event().start);
        assert_eq!(round.end, full_event().end);
        assert!(!round.all_day);
    }

    #[test]
    fn build_vcalendar_round_trips_an_all_day_event() {
        let mut event = full_event();
        event.all_day = true;
        event.start = "2026-07-20T00:00:00Z".parse().unwrap();
        event.end = "2026-07-21T00:00:00Z".parse().unwrap();
        let ics = build_vcalendar(&event);
        assert!(ics.contains("DTSTART;VALUE=DATE:20260720"), "all-day start should be a DATE value: {ics}");
        assert!(ics.contains("DTEND;VALUE=DATE:20260721"), "all-day end should be a DATE value: {ics}");
        let events = parse_vevents(&cal_id(), &ics);
        assert_eq!(events.len(), 1);
        assert!(events[0].all_day);
        assert_eq!(events[0].start, event.start);
        assert_eq!(events[0].end, event.end);
    }

    #[test]
    fn build_vcalendar_preserves_the_rrule_string() {
        let mut event = full_event();
        event.rrule = Some("FREQ=WEEKLY;COUNT=10".to_string());
        let ics = build_vcalendar(&event);
        assert!(ics.contains("RRULE:FREQ=WEEKLY;COUNT=10"), "RRULE should be serialized verbatim: {ics}");
        let events = parse_vevents(&cal_id(), &ics);
        assert_eq!(events[0].rrule.as_deref(), Some("FREQ=WEEKLY;COUNT=10"));
    }

    #[test]
    fn build_vcalendar_stamps_meta_from_parse_with_meta() {
        let ics = build_vcalendar(&full_event());
        let events = parse_vevents_with_meta(&cal_id(), &ics, Some("/cal/events/evt-2.ics"), Some("\"etag1\""));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].href.as_deref(), Some("/cal/events/evt-2.ics"));
        assert_eq!(events[0].etag.as_deref(), Some("\"etag1\""));
    }

    #[test]
    fn parses_recurrence_id_exdate_and_rdate_on_an_override() {
        // The master carries an EXDATE; the override replaces one instance
        // (its DTSTART differs from the instance it anchors, so the two are
        // distinguishable) with RANGE=THISANDFUTURE and an RDATE of its own.
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:ovr-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260706T090000Z\r\nDTEND:20260706T093000Z\r\nRRULE:FREQ=WEEKLY\r\nEXDATE:20260713T090000Z\r\nEXDATE;TZID=Europe/Berlin:20260720T110000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].exdates.len(), 2, "both EXDATEs parse, TZID-qualified one resolved to UTC");
        assert!(events[0].exdates.contains(&"2026-07-13T09:00:00Z".parse().unwrap()));
        assert!(events[0].exdates.contains(&"2026-07-20T09:00:00Z".parse().unwrap()), "Berlin 11:00 = UTC 09:00 (CEST)");

        let mut override_event = full_event();
        override_event.uid = events[0].uid.clone();
        override_event.start = "2026-07-20T10:00:00Z".parse().unwrap();
        override_event.end = "2026-07-20T10:30:00Z".parse().unwrap();
        override_event.recurrence_id = Some("2026-07-20T09:00:00Z".parse().unwrap());
        override_event.recurrence_range = RecurrenceRange::ThisAndFuture;
        override_event.rdates = vec!["2026-07-25T09:00:00Z".parse().unwrap()];
        let ics = build_vcalendar(&override_event);
        assert!(ics.contains("RECURRENCE-ID;RANGE=THISANDFUTURE:20260720T090000Z"), "override anchor + range: {ics}");
        assert!(ics.contains("RDATE:20260725T090000Z"), "rdate round-trips: {ics}");
        let round = &parse_vevents(&cal_id(), &ics)[0];
        assert_eq!(round.recurrence_id, override_event.recurrence_id);
        assert_eq!(round.recurrence_range, RecurrenceRange::ThisAndFuture);
        assert_eq!(round.rdates, override_event.rdates);
    }

    #[test]
    fn build_vcalendar_round_trips_exdates_on_an_all_day_master() {
        let mut event = full_event();
        event.all_day = true;
        event.start = "2026-07-20T00:00:00Z".parse().unwrap();
        event.end = "2026-07-21T00:00:00Z".parse().unwrap();
        event.exdates = vec!["2026-07-27T00:00:00Z".parse().unwrap()];
        let ics = build_vcalendar(&event);
        assert!(ics.contains("EXDATE;VALUE=DATE:20260727"), "all-day exdate is a DATE value: {ics}");
        let round = &parse_vevents(&cal_id(), &ics)[0];
        assert_eq!(round.exdates, event.exdates, "all-day EXDATE round-trips through the DATE parse");
    }

    #[test]
    fn parses_attendees_with_role_and_partstat() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:att-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;CN=Alice:mailto:alice@example.com\r\nATTENDEE;ROLE=OPT-PARTICIPANT:mailto:bob@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        let attendees = &events[0].attendees;
        assert_eq!(attendees.len(), 2);
        assert_eq!(attendees[0].address.address, "alice@example.com");
        assert_eq!(attendees[0].address.name.as_deref(), Some("Alice"));
        assert_eq!(attendees[0].role, AttendeeRole::Required);
        assert_eq!(attendees[0].status, AttendeeStatus::Accepted);
        assert_eq!(attendees[1].address.address, "bob@example.com");
        assert_eq!(attendees[1].role, AttendeeRole::Optional);
        assert_eq!(attendees[1].status, AttendeeStatus::NeedsAction);
    }

    #[test]
    fn parses_categories_as_comma_split_list() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:cat-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nCATEGORIES:Work,Important\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events[0].categories, vec!["Work".to_string(), "Important".to_string()]);
    }

    #[test]
    fn parses_class_and_transp() {
        let ics_absent = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:cls-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics_absent);
        assert_eq!(events[0].sensitivity, EventSensitivity::Public);
        assert_eq!(events[0].transparency, EventTransparency::Busy);

        let ics_set = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:cls-2@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nCLASS:CONFIDENTIAL\r\nTRANSP:TRANSPARENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics_set);
        assert_eq!(events[0].sensitivity, EventSensitivity::Confidential);
        assert_eq!(events[0].transparency, EventTransparency::Free);
    }

    #[test]
    fn parses_conference_url_preferring_conference_over_url() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:conf-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nCONFERENCE:https://example.com/join/abc\r\nURL:https://example.com/legacy\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events[0].conference_url.as_deref(), Some("https://example.com/join/abc"));
    }

    #[test]
    fn parses_valarm_trigger_as_reminder_minutes() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:alarm-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Reminder\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events[0].reminder_minutes_before, Some(15));
    }

    #[test]
    fn build_vcalendar_round_trips_attendees_categories_sensitivity_transparency_conference_and_reminder() {
        let mut event = full_event();
        event.attendees = vec![
            Attendee {
                address: EmailAddress {
                    name: Some("Alice".to_string()),
                    address: "alice@example.com".to_string(),
                },
                role: AttendeeRole::Required,
                status: AttendeeStatus::Accepted,
            },
            Attendee {
                address: EmailAddress::new("bob@example.com"),
                role: AttendeeRole::Optional,
                status: AttendeeStatus::NeedsAction,
            },
        ];
        event.organizer = Some(EmailAddress::new("organizer@example.com"));
        event.categories = vec!["Work".to_string(), "Important".to_string()];
        event.sensitivity = EventSensitivity::Private;
        event.transparency = EventTransparency::Free;
        event.conference_url = Some("https://example.com/join/abc".to_string());
        event.reminder_minutes_before = Some(15);

        let ics = build_vcalendar(&event);
        assert!(ics.contains("ORGANIZER"), "organizer should be written when attendees are present: {ics}");
        let events = parse_vevents(&cal_id(), &ics);
        assert_eq!(events.len(), 1);
        let round = &events[0];
        assert_eq!(round.attendees.len(), 2);
        assert_eq!(round.attendees[0].address.address, "alice@example.com");
        assert_eq!(round.attendees[0].address.name.as_deref(), Some("Alice"));
        assert_eq!(round.attendees[0].status, AttendeeStatus::Accepted);
        assert_eq!(round.attendees[1].role, AttendeeRole::Optional);
        assert_eq!(round.organizer.as_ref().map(|o| o.address.as_str()), Some("organizer@example.com"));
        assert_eq!(round.categories, event.categories);
        assert_eq!(round.sensitivity, EventSensitivity::Private);
        assert_eq!(round.transparency, EventTransparency::Free);
        assert_eq!(round.conference_url, event.conference_url);
        assert_eq!(round.reminder_minutes_before, Some(15));
    }

    #[test]
    fn build_vcalendar_omits_organizer_when_no_attendees() {
        let mut event = full_event();
        event.organizer = Some(EmailAddress::new("organizer@example.com"));
        let ics = build_vcalendar(&event);
        assert!(!ics.contains("ORGANIZER"), "organizer shouldn't be written without attendees: {ics}");
    }

    #[test]
    fn build_vcalendar_writes_default_class_and_transp_only_when_non_default() {
        let ics = build_vcalendar(&full_event());
        assert!(!ics.contains("CLASS"), "default Public sensitivity shouldn't add a CLASS line: {ics}");
        assert!(!ics.contains("TRANSP"), "default Busy transparency shouldn't add a TRANSP line: {ics}");
    }

    /// The CalDAV PUT body must never carry `METHOD` (it's an iMIP-only
    /// property), while the iMIP payload must - and the event content must
    /// round-trip identically between the two.
    #[test]
    fn imip_serialization_adds_method_only_in_the_email_form() {
        let event = full_event();
        let stored = build_vcalendar(&event);
        assert!(!stored.to_uppercase().contains("METHOD:"), "CalDAV body must not carry METHOD: {stored}");

        for (method, expected) in [(ImipMethod::Request, "REQUEST"), (ImipMethod::Reply, "REPLY"), (ImipMethod::Cancel, "CANCEL")] {
            let ics = build_imip_vcalendar(&event, method);
            assert!(ics.contains(&format!("METHOD:{expected}")), "expected METHOD:{expected} in:\n{ics}");
            assert_eq!(parse_imip_method(&ics), method);
            let events = parse_vevents(&cal_id(), &ics);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].uid.0, event.uid.0);
            assert_eq!(events[0].start, event.start);
        }
    }

    #[test]
    fn parse_imip_invitation_extracts_method_summary_and_organizer() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:inv-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Planning\r\nORGANIZER;CN=Ada:mailto:ada@example.com\r\nATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION:mailto:bob@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let invitation = parse_imip_invitation(ics).expect("parses");
        assert_eq!(invitation.method, ImipMethod::Request);
        assert_eq!(invitation.ics, ics);
        assert_eq!(invitation.summary.as_deref(), Some("Planning"));
        assert_eq!(invitation.organizer.as_ref().map(|o| o.address.as_str()), Some("ada@example.com"));
        assert_eq!(invitation.in_reply_to, None, "the message's own Message-ID is the reading pane's job");
        assert_eq!(invitation.start, "2026-07-15T14:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(invitation.end, "2026-07-15T15:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert!(!invitation.all_day);
        assert_eq!(invitation.location, None);
        assert_eq!(invitation.description, None);
        assert_eq!(invitation.rrule, None);
    }

    #[test]
    fn parse_imip_invitation_carries_location_description_and_recurrence() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:inv-2@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART;VALUE=DATE:20260810\r\nDTEND;VALUE=DATE:20260811\r\nSUMMARY:All-hands\r\nLOCATION:Room 7, Building C\r\nDESCRIPTION:Quarterly review with the whole team.\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let invitation = parse_imip_invitation(ics).expect("parses");
        assert!(invitation.all_day, "VALUE=DATE start must mark the event all-day");
        assert_eq!(invitation.start, "2026-08-10T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(invitation.end, "2026-08-11T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(invitation.location.as_deref(), Some("Room 7, Building C"));
        assert_eq!(invitation.description.as_deref(), Some("Quarterly review with the whole team."));
        assert_eq!(invitation.rrule.as_deref(), Some("FREQ=WEEKLY;COUNT=4"));
    }

    #[test]
    fn parse_imip_invitation_reports_reply_and_cancel_methods() {
        let cancel = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:CANCEL\r\nBEGIN:VEVENT\r\nUID:inv-1@example.com\r\nDTSTAMP:20260102T090000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Planning\r\nORGANIZER:mailto:ada@example.com\r\nATTENDEE:mailto:bob@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let invitation = parse_imip_invitation(cancel).expect("parses");
        assert_eq!(invitation.method, ImipMethod::Cancel);
        assert_eq!(invitation.summary.as_deref(), Some("Planning"));
    }

    #[test]
    fn parse_imip_invitation_returns_none_for_a_document_without_a_vevent() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n";
        assert_eq!(parse_imip_invitation(ics), None);
        assert_eq!(parse_imip_invitation("not an icalendar document"), None);
    }

    /// An Outlook/Exchange Online invitation (the shape a Teams meeting
    /// invite arrives in): `VTIMEZONE` with a Windows timezone ID, a quoted
    /// `TZID` on `DTSTART`/`DTEND`, `SENT-BY` on the organizer, and
    /// `X-MICROSOFT-CDO-*` properties. The Windows ID isn't an IANA name, so
    /// the pre-fix parser dropped the event and the reading pane showed no
    /// banner - the CLDR mapping must resolve it (with the DST offset the
    /// organizer's zone has that day) instead.
    #[test]
    fn parse_imip_invitation_resolves_windows_timezone_ids() {
        let ics = "BEGIN:VCALENDAR\r\n\
PRODID:Microsoft Exchange Server 2019\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:W. Europe Standard Time\r\n\
BEGIN:STANDARD\r\n\
DTSTART:16011001T030000\r\n\
RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=10\r\n\
TZOFFSETFROM:+0200\r\n\
TZOFFSETTO:+0100\r\n\
END:STANDARD\r\n\
BEGIN:DAYLIGHT\r\n\
DTSTART:16010301T020000\r\n\
RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=3\r\n\
TZOFFSETFROM:+0100\r\n\
TZOFFSETTO:+0200\r\n\
END:DAYLIGHT\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\n\
ORGANIZER;CN=Ada Lovelace;SENT-BY=\"mailto:ada@contoso.com\":mailto:ada@contoso.com\r\n\
ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;CN=Bob Example;RSVP=TRUE:mailto:bob@example.com\r\n\
UID:040000008200E00074C5B7101A82E008000000001AC05E4F0B88D701000000000000000010000000F8B2B12E34C5B94A9B2B2E3B00C47E00\r\n\
DTSTAMP:20260801T120000Z\r\n\
DTSTART;TZID=\"W. Europe Standard Time\":20260810T100000\r\n\
DTEND;TZID=\"W. Europe Standard Time\":20260810T103000\r\n\
LOCATION:https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc123%40thread.v2/0\r\n\
SUMMARY:Weekly design sync (Teams)\r\n\
TRANSP:OPAQUE\r\n\
X-MICROSOFT-CDO-APPT-SEQUENCE:0\r\n\
X-MICROSOFT-CDO-BUSYSTATUS:BUSY\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let invitation = parse_imip_invitation(ics).expect("Outlook invitation must parse");
        assert_eq!(invitation.method, ImipMethod::Request);
        assert_eq!(invitation.summary.as_deref(), Some("Weekly design sync (Teams)"));
        assert_eq!(invitation.organizer.as_ref().map(|o| o.address.as_str()), Some("ada@contoso.com"));
        // 10 August 2026 falls in DST: W. Europe Standard Time = Europe/Berlin
        // (CEST, UTC+2), so 10:00 local is 08:00 UTC - not 10:00.
        assert_eq!(invitation.start, "2026-08-10T08:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(invitation.end, "2026-08-10T08:30:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(invitation.location.as_deref(), Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc123%40thread.v2/0"));
        assert_eq!(invitation.rrule, None);
    }

    /// The Outlook *desktop* form of the same invitation: `TZID` unquoted
    /// (`DTSTART;TZID=W. Europe Standard Time:...`, technically invalid RFC
    /// 5545 but what Outlook emits), on a winter date so the resolved offset
    /// is the standard one (CET, UTC+1) - DST rules must follow the zone, not
    /// a fixed offset.
    #[test]
    fn parse_imip_invitation_resolves_unquoted_windows_tzid_outside_dst() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:inv-winter@contoso.com\r\nDTSTAMP:20260110T090000Z\r\nDTSTART;TZID=W. Europe Standard Time:20260115T140000\r\nDTEND;TZID=W. Europe Standard Time:20260115T143000\r\nSUMMARY:January sync\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let invitation = parse_imip_invitation(ics).expect("Outlook invitation must parse");
        assert_eq!(invitation.start, "2026-01-15T13:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(invitation.end, "2026-01-15T13:30:00Z".parse::<DateTime<Utc>>().unwrap());
    }

    /// A `TZID` that is neither IANA nor a known Windows ID stays
    /// unresolvable: the event is dropped (and logged), matching the
    /// pre-fix behavior for unknown zones - the alternative, guessing an
    /// offset, would show the wrong meeting time with no way to tell.
    #[test]
    fn parse_imip_invitation_drops_an_unknown_timezone() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:inv-tz@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART;TZID=Fictional Standard Time:20260715T140000\r\nSUMMARY:Elsewhere\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(parse_imip_invitation(ics), None);
    }

    /// IANA `TZID`s (the form Google Calendar and iCloud invitations use)
    /// still resolve through chrono-tz directly - the Windows lookup must not
    /// shadow them.
    #[test]
    fn parse_imip_invitation_still_resolves_iana_timezones() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:inv-iana@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART;TZID=Europe/Berlin:20260715T140000\r\nDTEND;TZID=Europe/Berlin:20260715T150000\r\nSUMMARY:IANA sync\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let invitation = parse_imip_invitation(ics).expect("IANA-tzid invitation must parse");
        assert_eq!(invitation.start, "2026-07-15T12:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(invitation.end, "2026-07-15T13:00:00Z".parse::<DateTime<Utc>>().unwrap());
    }

    /// The shared `test-fixtures/holidays.ics`: a multi-VEVENT document with
    /// an all-day event, a recurring event, and a VTODO that must be ignored -
    /// the shape a webcal feed or an imported `.ics` file takes. Exercises
    /// the exact path the subscription session and import dialog feed into.
    #[test]
    fn parses_the_multi_vevent_fixture_ignoring_todos() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/holidays.ics");
        let ics = std::fs::read_to_string(path).expect("fixture exists");
        let events = parse_vevents(&cal_id(), &ics);
        assert_eq!(events.len(), 2, "the VTODO must be ignored, only VEVENTs parsed");
        assert_eq!(events[0].uid.0, "holiday-1@example.com");
        assert!(events[0].all_day, "VALUE=DATE start must mark the event all-day");
        assert_eq!(events[0].start, "2026-08-03T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(events[1].uid.0, "weekly-2@example.com");
        assert_eq!(events[1].rrule.as_deref(), Some("FREQ=WEEKLY;COUNT=4"));
        assert!(!events[1].all_day);
        // Fixture events must carry no write metadata - they're create-only
        // until an import/upsert stamps them.
        assert!(events.iter().all(|e| e.href.is_none() && e.etag.is_none()));
    }

    #[test]
    fn parses_plain_vtodo_with_due_and_start() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VTODO\r\nUID:todo-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T090000Z\r\nDUE:20260715T170000Z\r\nSUMMARY:File the report\r\nDESCRIPTION:Quarterly report\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let tasks = parse_vtodos(&cal_id(), ics);
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert_eq!(task.uid.0, "todo-1@example.com");
        assert_eq!(task.summary.as_deref(), Some("File the report"));
        assert_eq!(task.description.as_deref(), Some("Quarterly report"));
        assert_eq!(task.due, Some("2026-07-15T17:00:00Z".parse().unwrap()));
        assert_eq!(task.start, Some("2026-07-15T09:00:00Z".parse().unwrap()));
        assert_eq!(task.status, TaskStatus::NeedsAction);
        assert_eq!(task.priority, TaskPriority(0));
        assert_eq!(task.percent_complete, None);
        assert_eq!(task.completed, None);
    }

    #[test]
    fn parses_completed_todo_with_status_priority_and_percent() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VTODO\r\nUID:done-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDUE:20260714T170000Z\r\nSUMMARY:Send invoice\r\nSTATUS:COMPLETED\r\nCOMPLETED:20260713T110000Z\r\nPRIORITY:5\r\nPERCENT-COMPLETE:100\r\nCATEGORIES:Work,Billing\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let tasks = parse_vtodos(&cal_id(), ics);
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.completed, Some("2026-07-13T11:00:00Z".parse().unwrap()));
        assert_eq!(task.priority, TaskPriority(5));
        assert_eq!(task.percent_complete, Some(100));
        assert_eq!(task.categories, vec!["Work".to_string(), "Billing".to_string()]);
    }

    #[test]
    fn maps_in_progress_and_unknown_statuses() {
        let in_progress = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:wip@example.com\r\nDTSTAMP:20260101T000000Z\r\nSUMMARY:WIP\r\nSTATUS:IN-PROCESS\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let tasks = parse_vtodos(&cal_id(), in_progress);
        assert_eq!(tasks[0].status, TaskStatus::InProgress);

        // A server-emitted status we don't model falls back to NeedsAction.
        let cancelled =
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:cx@example.com\r\nDTSTAMP:20260101T000000Z\r\nSUMMARY:CX\r\nSTATUS:CANCELLED\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let tasks = parse_vtodos(&cal_id(), cancelled);
        assert_eq!(tasks[0].status, TaskStatus::Cancelled);
    }

    #[test]
    fn duration_based_todo_computes_due_from_start() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VTODO\r\nUID:dur-todo@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T090000Z\r\nDURATION:PT4H\r\nSUMMARY:Half-day task\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let tasks = parse_vtodos(&cal_id(), ics);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].due, Some("2026-07-15T13:00:00Z".parse().unwrap()));
        assert_eq!(tasks[0].start, Some("2026-07-15T09:00:00Z".parse().unwrap()));
    }

    #[test]
    fn todo_with_no_temporal_anchor_parses_without_due() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VTODO\r\nUID:anchorless@example.com\r\nDTSTAMP:20260101T000000Z\r\nSUMMARY:No date\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let tasks = parse_vtodos(&cal_id(), ics);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].due, None);
        assert_eq!(tasks[0].start, None);
    }

    #[test]
    fn malformed_calendar_data_yields_no_tasks_rather_than_panicking() {
        let tasks = parse_vtodos(&cal_id(), "this is not iCalendar data at all");
        assert!(tasks.is_empty());
    }

    fn full_task() -> CalendarTask {
        CalendarTask {
            uid: TaskUid("task-1@example.com".to_string()),
            calendar_id: cal_id(),
            summary: Some("Book the venue".to_string()),
            description: Some("Call and confirm".to_string()),
            due: Some("2026-07-20T17:00:00Z".parse().unwrap()),
            start: Some("2026-07-15T09:00:00Z".parse().unwrap()),
            completed: None,
            status: TaskStatus::NeedsAction,
            priority: TaskPriority(3),
            percent_complete: Some(40),
            categories: vec!["Work".to_string()],
            href: None,
            etag: None,
        }
    }

    #[test]
    fn build_vtodo_calendar_round_trips_a_task() {
        let ics = build_vtodo_calendar(&full_task());
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"), "should be CRLF line endings, got: {ics}");
        assert!(ics.contains("BEGIN:VTODO"), "should contain a VTODO component: {ics}");
        let tasks = parse_vtodos(&cal_id(), &ics);
        assert_eq!(tasks.len(), 1);
        let round = &tasks[0];
        assert_eq!(round.uid, full_task().uid);
        assert_eq!(round.summary, full_task().summary);
        assert_eq!(round.description, full_task().description);
        assert_eq!(round.due, full_task().due);
        assert_eq!(round.start, full_task().start);
        assert_eq!(round.priority, full_task().priority);
        assert_eq!(round.percent_complete, full_task().percent_complete);
        assert_eq!(round.categories, full_task().categories);
        assert_eq!(round.status, TaskStatus::NeedsAction);
    }

    #[test]
    fn build_vtodo_calendar_round_trips_completed_status_and_timestamp() {
        let mut task = full_task();
        task.status = TaskStatus::Completed;
        task.completed = Some("2026-07-19T10:00:00Z".parse().unwrap());
        task.percent_complete = Some(100);
        let ics = build_vtodo_calendar(&task);
        assert!(ics.contains("STATUS:COMPLETED"), "status must be written: {ics}");
        let tasks = parse_vtodos(&cal_id(), &ics);
        assert_eq!(tasks[0].status, TaskStatus::Completed);
        assert_eq!(tasks[0].completed, task.completed);
    }

    #[test]
    fn build_vtodo_calendar_omits_unset_fields() {
        let mut task = full_task();
        task.due = None;
        task.start = None;
        task.priority = TaskPriority(0);
        task.percent_complete = None;
        let ics = build_vtodo_calendar(&task);
        assert!(!ics.contains("DUE"), "no DUE line expected: {ics}");
        assert!(!ics.contains("DTSTART"), "no DTSTART line expected: {ics}");
        assert!(!ics.contains("PRIORITY"), "default priority must be omitted: {ics}");
        assert!(!ics.contains("PERCENT-COMPLETE"), "unset percent must be omitted: {ics}");
        assert!(!ics.contains("STATUS"), "NeedsAction is the RFC default, no STATUS line expected: {ics}");
        assert!(!ics.contains("COMPLETED"), "no COMPLETED line expected: {ics}");
        let tasks = parse_vtodos(&cal_id(), &ics);
        assert_eq!(tasks[0].due, None);
        assert_eq!(tasks[0].status, TaskStatus::NeedsAction);
    }

    #[test]
    fn build_vtodo_calendar_stamps_meta_from_parse_with_meta() {
        let ics = build_vtodo_calendar(&full_task());
        let tasks = parse_vtodos_with_meta(&cal_id(), &ics, Some("/cal/tasks/task-1.ics"), Some("\"etag1\""));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].href.as_deref(), Some("/cal/tasks/task-1.ics"));
        assert_eq!(tasks[0].etag.as_deref(), Some("\"etag1\""));
    }

    #[test]
    fn parses_the_multi_vevent_fixture_todo() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/holidays.ics");
        let ics = std::fs::read_to_string(path).expect("fixture exists");
        let tasks = parse_vtodos(&cal_id(), &ics);
        assert_eq!(tasks.len(), 1, "only the VTODO is a task - the VEVENTs are ignored");
        assert_eq!(tasks[0].uid.0, "todo-1@example.com");
        assert_eq!(tasks[0].summary.as_deref(), Some("Ignored todo"));
        assert_eq!(tasks[0].due, None);
        assert!(tasks[0].href.is_none() && tasks[0].etag.is_none());
    }
}
