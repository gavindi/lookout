/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
// Organizer-side invitation logic for calendar saves and deletes
// (OUTLOOK_PARITY.md item 3.4 / CAL-14). Pure decisions and message
// construction live here so they can be unit-tested without a GTK window;
// `window.rs` owns the state lookups, prompts and actor dispatch.
//
// Delivery has two backends per OUTLOOK_PARITY §4.5: a CalDAV server that
// advertises RFC 6638 scheduling (`schedule-outbox-URL`) delivers iTIP
// itself, so this client stands down; otherwise the organizer's own mail
// account sends iMIP REQUEST/CANCEL through the normal outbox.
use lookout_core::{Attendee, CalendarEvent, EmailAddress, EventOccurrence, ImipMethod, RecurrenceRange, ScheduleAgent, SchedulingInfo};
use lookout_mail::ComposedMessage;

/// Who delivers a meeting's iTIP messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The CalDAV server schedules implicitly (RFC 6638): saving the event is
    /// all that's needed, so the client sends no iMIP.
    Server,
    /// The client sends iMIP itself through the organizer's mail account.
    Client,
}

/// Resolves the delivery backend from the account's discovered scheduling
/// capability and the event's own override. RFC 6638 §7.1: an explicit
/// `SCHEDULE-AGENT=CLIENT` keeps delivery on this client even when the server
/// could schedule.
pub fn delivery_for(scheduling: &SchedulingInfo, schedule_agent: Option<ScheduleAgent>) -> Delivery {
    if scheduling.supports_server_scheduling() && schedule_agent != Some(ScheduleAgent::Client) {
        Delivery::Server
    } else {
        Delivery::Client
    }
}

/// What a save should do about attendees once delivery is `Client`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveInvite {
    /// No attendees before or after, or the server schedules: nothing to do.
    None,
    /// A brand-new meeting: send the invitation straight away.
    Request,
    /// An edit to an existing meeting: ask "Send update to attendees?" then
    /// send the (SEQUENCE-bumped) update.
    PromptUpdate,
    /// The attendee list was emptied: ask whether to cancel the removed
    /// attendees.
    PromptCancel,
}

/// Classifies a save. Callers must have already resolved `delivery`: with
/// `Delivery::Server` there is nothing to classify (the server schedules).
pub fn classify_save(previous: Option<&EventOccurrence>, next: &CalendarEvent) -> SaveInvite {
    let had_attendees = previous.is_some_and(|previous| !previous.attendees.is_empty());
    if next.attendees.is_empty() {
        return if had_attendees { SaveInvite::PromptCancel } else { SaveInvite::None };
    }
    if previous.is_none() {
        SaveInvite::Request
    } else {
        SaveInvite::PromptUpdate
    }
}

/// What a delete should do about attendees once delivery is `Client`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteInvite {
    None,
    /// Ask "Send cancellation?" - the delete itself proceeds either way.
    PromptCancel,
}

pub fn classify_delete(occ: &EventOccurrence, delivery: Delivery) -> DeleteInvite {
    if delivery == Delivery::Server || occ.attendees.is_empty() {
        DeleteInvite::None
    } else {
        DeleteInvite::PromptCancel
    }
}

/// The organizer's own address, used as `From` on the iMIP message and as the
/// fallback `ORGANIZER` in a cancellation built from a cached occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizerIdentity {
    pub email: String,
    pub display_name: Option<String>,
}

/// The three iTIP verbs the organizer sends, with Outlook's subject forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteKind {
    Request,
    Update,
    Cancel,
}

impl InviteKind {
    pub fn method(self) -> ImipMethod {
        match self {
            InviteKind::Request | InviteKind::Update => ImipMethod::Request,
            InviteKind::Cancel => ImipMethod::Cancel,
        }
    }

    fn subject_prefix(self) -> &'static str {
        match self {
            InviteKind::Request => "Invitation",
            InviteKind::Update => "Updated invitation",
            InviteKind::Cancel => "Cancelled",
        }
    }

    fn body(self, summary: &str) -> String {
        match self {
            InviteKind::Request => format!("You have been invited to \"{summary}\"."),
            InviteKind::Update => format!("\"{summary}\" has been updated."),
            InviteKind::Cancel => format!("\"{summary}\" has been cancelled."),
        }
    }
}

/// Builds one iMIP message (RFC 6047): a `multipart/alternative`
/// [text/plain, text/calendar; method=...] carrying the calendar part the
/// mail crate encodes. Shared with the attendee REPLY path so both ends of
/// the wire build identically.
#[allow(clippy::too_many_arguments)]
pub fn build_imip_message(
    event: &CalendarEvent,
    method: ImipMethod,
    from: &OrganizerIdentity,
    to: Vec<String>,
    subject: String,
    text_body: String,
    in_reply_to: Option<String>,
) -> ComposedMessage {
    ComposedMessage {
        from: from.email.clone(),
        display_name: from.display_name.clone(),
        to,
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject,
        importance: lookout_core::Importance::default(),
        text_body,
        html_body: None,
        attachments: Vec::new(),
        inline_images: Vec::new(),
        calendar_part: Some(lookout_dav::build_imip_vcalendar(event, method)),
        read_receipt: None,
        request_read_receipt: false,
        in_reply_to,
        references: Vec::new(),
        message_id: None,
    }
}

/// The organizer's outgoing invitation/update/cancellation. Recipients are
/// the attendee list with the organizer's own address removed (a user can be
/// on their own attendee list; iMIP doesn't need to mail you yourself).
pub fn invite_message(event: &CalendarEvent, kind: InviteKind, identity: &OrganizerIdentity) -> ComposedMessage {
    let summary = event.summary.as_deref().map(str::trim).filter(|summary| !summary.is_empty()).unwrap_or("(No subject)");
    let subject = format!("{}: {summary}", kind.subject_prefix());
    build_imip_message(
        event,
        kind.method(),
        identity,
        recipient_addresses(&event.attendees, &identity.email),
        subject,
        kind.body(summary),
        None,
    )
}

/// Whether this account is the event's organizer, and so may send
/// invitations for it: an absent `ORGANIZER` counts as ours (an event created
/// here before the owner address could be resolved), while a different
/// address means the event arrived from someone else - deleting or editing it
/// must not mail anyone on that organizer's behalf.
pub fn is_organizer(organizer: Option<&EmailAddress>, identity_email: &str) -> bool {
    organizer.is_none_or(|organizer| organizer.address.eq_ignore_ascii_case(identity_email))
}

/// Every attendee address except `self_email` (case-insensitive).
pub fn recipient_addresses(attendees: &[Attendee], self_email: &str) -> Vec<String> {
    attendees
        .iter()
        .map(|attendee| attendee.address.address.clone())
        .filter(|address| !address.eq_ignore_ascii_case(self_email))
        .collect()
}

/// Rebuilds a cancellation-ready event from a cached occurrence: the fields a
/// CANCEL VEVENT needs (UID, times, organizer, attendees, SEQUENCE) plus a
/// `RECURRENCE-ID` naming the withdrawn instance when the occurrence belongs
/// to a series - a plain expansion's instance time, or the override's own
/// anchor. The series `RRULE` itself is deliberately not carried: a
/// cancellation names the withdrawn instance, not the pattern.
pub fn cancellation_event(occ: &EventOccurrence, organizer: Option<EmailAddress>) -> CalendarEvent {
    CalendarEvent {
        uid: occ.uid.clone(),
        calendar_id: occ.calendar_id.clone(),
        summary: occ.summary.clone(),
        description: occ.description.clone(),
        location: occ.location.clone(),
        start: occ.start,
        end: occ.end,
        all_day: occ.all_day,
        time_zone: occ.time_zone.clone(),
        rrule: None,
        recurrence_id: occ.recurrence_id.or_else(|| occ.rrule.as_ref().map(|_| occ.start)),
        recurrence_range: RecurrenceRange::default(),
        exdates: Vec::new(),
        rdates: Vec::new(),
        href: occ.href.clone(),
        etag: occ.etag.clone(),
        sequence: occ.sequence,
        attendees: occ.attendees.clone(),
        organizer: occ.organizer.clone().or(organizer),
        schedule_agent: occ.schedule_agent,
        categories: occ.categories.clone(),
        sensitivity: occ.sensitivity,
        show_as: occ.show_as,
        reminder_minutes_before: None,
        conference_url: occ.conference_url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::{AttendeeRole, AttendeeStatus, BusyStatus, CalendarId, EventSensitivity, EventUid, ImipMethod};
    use std::str::FromStr;

    fn attendee(address: &str) -> Attendee {
        Attendee {
            address: EmailAddress::new(address.to_string()),
            role: AttendeeRole::Required,
            status: AttendeeStatus::NeedsAction,
        }
    }

    fn event(summary: &str, attendees: Vec<Attendee>) -> CalendarEvent {
        CalendarEvent {
            uid: EventUid("evt-1@example.com".to_string()),
            calendar_id: CalendarId("acct:cal".to_string()),
            summary: Some(summary.to_string()),
            description: None,
            location: None,
            start: "2026-08-12T09:00:00Z".parse().unwrap(),
            end: "2026-08-12T10:00:00Z".parse().unwrap(),
            all_day: false,
            time_zone: None,
            rrule: None,
            recurrence_id: None,
            recurrence_range: RecurrenceRange::default(),
            exdates: Vec::new(),
            rdates: Vec::new(),
            href: None,
            etag: None,
            sequence: 4,
            attendees,
            organizer: Some(EmailAddress::new("me@example.com".to_string())),
            schedule_agent: None,
            categories: Vec::new(),
            sensitivity: EventSensitivity::default(),
            show_as: BusyStatus::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    fn occurrence(attendees: Vec<Attendee>, rrule: Option<&str>) -> EventOccurrence {
        EventOccurrence {
            uid: EventUid("evt-1@example.com".to_string()),
            calendar_id: CalendarId("acct:cal".to_string()),
            summary: Some("Review".to_string()),
            description: None,
            location: None,
            start: "2026-08-12T09:00:00Z".parse().unwrap(),
            end: "2026-08-12T10:00:00Z".parse().unwrap(),
            all_day: false,
            time_zone: None,
            rrule: rrule.map(str::to_string),
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
            sequence: 4,
            attendees,
            organizer: Some(EmailAddress::new("me@example.com".to_string())),
            schedule_agent: None,
            categories: Vec::new(),
            sensitivity: EventSensitivity::default(),
            show_as: BusyStatus::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    #[test]
    fn delivery_prefers_the_server_unless_the_event_says_client() {
        let scheduling = SchedulingInfo {
            schedule_outbox: Some("/outbox/".to_string()),
            schedule_inbox: None,
        };
        assert_eq!(delivery_for(&scheduling, None), Delivery::Server);
        assert_eq!(delivery_for(&scheduling, Some(ScheduleAgent::Server)), Delivery::Server);
        assert_eq!(delivery_for(&scheduling, Some(ScheduleAgent::Client)), Delivery::Client);
        assert_eq!(delivery_for(&SchedulingInfo::default(), None), Delivery::Client, "no outbox means the client schedules");
        assert_eq!(
            delivery_for(&SchedulingInfo::default(), Some(ScheduleAgent::Server)),
            Delivery::Client,
            "a SERVER hint without an outbox still falls back to client iMIP"
        );
    }

    #[test]
    fn classify_save_requests_new_meetings_and_prompts_on_changes() {
        let invitees = vec![attendee("alice@example.com")];
        assert_eq!(classify_save(None, &event("Planning", invitees.clone())), SaveInvite::Request);
        assert_eq!(classify_save(None, &event("Focus block", Vec::new())), SaveInvite::None);
        assert_eq!(
            classify_save(Some(&occurrence(invitees.clone(), None)), &event("Planning", invitees.clone())),
            SaveInvite::PromptUpdate
        );
        assert_eq!(
            classify_save(Some(&occurrence(Vec::new(), None)), &event("Planning", invitees)),
            SaveInvite::PromptUpdate,
            "adding the first attendee to an existing event is still an update to send"
        );
        assert_eq!(
            classify_save(Some(&occurrence(vec![attendee("alice@example.com")], None)), &event("Planning", Vec::new())),
            SaveInvite::PromptCancel
        );
    }

    #[test]
    fn classify_delete_requires_a_meeting_and_client_delivery() {
        let with_attendees = occurrence(vec![attendee("alice@example.com")], None);
        let without = occurrence(Vec::new(), None);
        assert_eq!(classify_delete(&with_attendees, Delivery::Client), DeleteInvite::PromptCancel);
        assert_eq!(classify_delete(&with_attendees, Delivery::Server), DeleteInvite::None);
        assert_eq!(classify_delete(&without, Delivery::Client), DeleteInvite::None);
    }

    #[test]
    fn is_organizer_accepts_our_own_address_and_missing_organizer_only() {
        let ours = EmailAddress::new("me@example.com".to_string());
        let theirs = EmailAddress::new("someone@else.example".to_string());
        assert!(is_organizer(Some(&ours), "ME@example.com"), "comparison ignores case");
        assert!(is_organizer(None, "me@example.com"), "a missing organizer is treated as ours");
        assert!(!is_organizer(Some(&theirs), "me@example.com"), "someone else's meeting is not ours to update");
    }

    #[test]
    fn invite_message_carries_method_subject_and_recipients() {
        let identity = OrganizerIdentity {
            email: "me@example.com".to_string(),
            display_name: Some("Me".to_string()),
        };
        let meeting = event("Planning", vec![attendee("alice@example.com"), attendee("me@example.com"), attendee("bob@example.com")]);

        let request = invite_message(&meeting, InviteKind::Request, &identity);
        assert_eq!(request.from, "me@example.com");
        assert_eq!(request.subject, "Invitation: Planning");
        assert_eq!(
            request.to,
            vec!["alice@example.com".to_string(), "bob@example.com".to_string()],
            "the organizer's own address is not mailed"
        );
        let ics = request.calendar_part.as_deref().unwrap();
        assert_eq!(lookout_core::parse_imip_method(ics), ImipMethod::Request);
        assert!(ics.contains("SEQUENCE:4"), "{ics}");
        assert!(ics.contains("RSVP=TRUE"), "{ics}");
        assert!(ics.contains("X-MICROSOFT-CDO-APPT-SEQUENCE:4"), "{ics}");

        let update = invite_message(&meeting, InviteKind::Update, &identity);
        assert_eq!(update.subject, "Updated invitation: Planning");
        assert_eq!(lookout_core::parse_imip_method(update.calendar_part.as_deref().unwrap()), ImipMethod::Request);

        let cancel = invite_message(&meeting, InviteKind::Cancel, &identity);
        assert_eq!(cancel.subject, "Cancelled: Planning");
        let ics = cancel.calendar_part.as_deref().unwrap();
        assert_eq!(lookout_core::parse_imip_method(ics), ImipMethod::Cancel);
        assert!(ics.contains("STATUS:CANCELLED"), "{ics}");
        assert!(!ics.contains("RSVP=TRUE"), "{ics}");
    }

    #[test]
    fn invite_message_falls_back_for_an_untitled_event() {
        let identity = OrganizerIdentity {
            email: "me@example.com".to_string(),
            display_name: None,
        };
        let mut untitled = event("Untitled", vec![attendee("alice@example.com")]);
        untitled.summary = Some("   ".to_string());
        assert_eq!(invite_message(&untitled, InviteKind::Request, &identity).subject, "Invitation: (No subject)");
    }

    #[test]
    fn cancellation_event_anchors_instance_cancels_with_recurrence_id() {
        let identity_organizer = Some(EmailAddress::new("me@example.com".to_string()));

        let standalone = cancellation_event(&occurrence(vec![attendee("alice@example.com")], None), identity_organizer.clone());
        assert!(standalone.recurrence_id.is_none(), "a non-recurring cancellation names no instance");
        assert_eq!(standalone.sequence, 4);

        let series = cancellation_event(&occurrence(vec![attendee("alice@example.com")], Some("FREQ=WEEKLY")), identity_organizer.clone());
        assert_eq!(
            series.recurrence_id,
            Some(chrono::DateTime::from_str("2026-08-12T09:00:00Z").unwrap()),
            "a series instance is anchored by its own start"
        );
        assert!(series.rrule.is_none(), "a cancellation names the instance, not the rule");
    }
}
