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
use lookout_core::{Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, CalendarId, EmailAddress, EventOccurrence, ImipMethod, RecurrenceRange, ScheduleAgent, SchedulingInfo};
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
        sensitivity: lookout_core::MessageSensitivity::Normal,
        text_body,
        html_body: None,
        attachments: Vec::new(),
        inline_images: Vec::new(),
        calendar_part: Some(lookout_dav::build_imip_vcalendar(event, method)),
        read_receipt: None,
        request_read_receipt: false,
        request_delivery_receipt: false,
        in_reply_to,
        references: Vec::new(),
        message_id: None,
    }
}

/// The attendee-side response verbs (CAL-15), with Outlook's subject
/// prefixes. Pure text so the banner dialog, the invite card's buttons and
/// the composer path share exactly one wording.
fn reply_prefix(status: AttendeeStatus) -> &'static str {
    match status {
        AttendeeStatus::Accepted => "Accepted",
        AttendeeStatus::Tentative => "Tentatively accepted",
        AttendeeStatus::Declined => "Declined",
        AttendeeStatus::NeedsAction => "Replied",
    }
}

/// The iMIP REPLY's subject: "Accepted: Planning", or the bare verb for an
/// untitled event.
pub fn reply_subject(status: AttendeeStatus, summary: Option<&str>) -> String {
    let prefix = reply_prefix(status);
    match summary.map(str::trim).filter(|summary| !summary.is_empty()) {
        Some(summary) => format!("{prefix}: {summary}"),
        None => prefix.to_string(),
    }
}

/// The plain-text body of the iMIP REPLY: the attendee's optional comment
/// ahead of the status line, blank-line separated.
pub fn reply_text_body(status: AttendeeStatus, comment: Option<&str>) -> String {
    let status_line = format!("{}.", reply_prefix(status));
    match comment.map(str::trim).filter(|comment| !comment.is_empty()) {
        Some(comment) => format!("{comment}\n\n{status_line}"),
        None => status_line,
    }
}

/// The toast after a response. The "without sending" variants make it
/// explicit that no mail went to the organizer (the invite card's
/// "Don't send a response").
pub fn reply_toast(status: AttendeeStatus, sent: bool) -> &'static str {
    match (status, sent) {
        (AttendeeStatus::Accepted, true) => "Invitation accepted",
        (AttendeeStatus::Tentative, true) => "Marked as tentative",
        (AttendeeStatus::Declined, true) => "Invitation declined",
        (AttendeeStatus::NeedsAction, true) => "Reply sent",
        (AttendeeStatus::Accepted, false) => "Accepted without sending a response",
        (AttendeeStatus::Tentative, false) => "Marked as tentative without sending a response",
        (AttendeeStatus::Declined, false) => "Declined without sending a response",
        (AttendeeStatus::NeedsAction, false) => "Marked without sending a response",
    }
}

/// Stamps the user's own `PARTSTAT` onto their ATTENDEE line (the organizer's
/// client reads the reply from exactly this); a self-signed invitation that
/// omitted the recipient entirely gets an ATTENDEE added.
pub fn stamp_attendee_status(event: &mut CalendarEvent, from_email: &str, status: AttendeeStatus) {
    match event.attendees.iter_mut().find(|attendee| attendee.address.address.eq_ignore_ascii_case(from_email)) {
        Some(attendee) => attendee.status = status,
        None => event.attendees.push(Attendee {
            address: EmailAddress::new(from_email.to_string()),
            role: AttendeeRole::Required,
            status,
        }),
    }
}

/// Parses an invitation's `VEVENT` and stamps the user's answer onto their
/// ATTENDEE line: the event both the immediate reply and the edited-in-the-
/// composer reply send back.
pub fn reply_event(invitation_ics: &str, from_email: &str, status: AttendeeStatus) -> Option<CalendarEvent> {
    let mut event = lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), invitation_ics).into_iter().next()?;
    stamp_attendee_status(&mut event, from_email, status);
    Some(event)
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

/// The event's display summary with the shared "(No subject)" fallback.
fn summary_or_no_subject(event: &CalendarEvent) -> &str {
    event.summary.as_deref().map(str::trim).filter(|summary| !summary.is_empty()).unwrap_or("(No subject)")
}

/// The display name (or address) the proposal/forward text bodies name.
fn sender_label(identity: &OrganizerIdentity) -> &str {
    identity.display_name.as_deref().map(str::trim).filter(|name| !name.is_empty()).unwrap_or(&identity.email)
}

/// The attendee's time proposal (CAL-17): a `METHOD:COUNTER` email to the
/// organizer whose calendar document carries the proposed times on the
/// invitation's own event, and whose plain text names the proposer and the
/// proposed slot. `when` is the caller's preformatted slot label (the app
/// renders it in the calendar's display zone).
pub fn counter_message(
    event: &CalendarEvent,
    proposed_start: chrono::DateTime<chrono::Utc>,
    proposed_end: chrono::DateTime<chrono::Utc>,
    identity: &OrganizerIdentity,
    organizer: &EmailAddress,
    when: &str,
    in_reply_to: Option<String>,
) -> ComposedMessage {
    let mut proposal = event.clone();
    proposal.start = proposed_start;
    proposal.end = proposed_end;
    let summary = summary_or_no_subject(&proposal);
    let subject = format!("New time proposed: {summary}");
    let text_body = format!("{} proposed a new time for this meeting.\n\n{summary}\n{when}", sender_label(identity));
    build_imip_message(&proposal, ImipMethod::Counter, identity, vec![organizer.address.clone()], subject, text_body, in_reply_to)
}

/// The organizer's refusal of a proposal (CAL-17): a `METHOD:DECLINECOUNTER`
/// to the proposing attendee carrying the originally scheduled times (the
/// stored event), so their client can say the proposal was turned down.
pub fn decline_counter_message(event: &CalendarEvent, identity: &OrganizerIdentity, proposer: &EmailAddress) -> ComposedMessage {
    let summary = summary_or_no_subject(event);
    let subject = format!("Declined: {summary}");
    let text_body = format!("{summary} will stay at the originally scheduled time.");
    build_imip_message(event, ImipMethod::DeclineCounter, identity, vec![proposer.address.clone()], subject, text_body, None)
}

/// The attendee's forwarded meeting (CAL-18): the meeting REQUEST goes to the
/// new recipients with them added to the calendar document's attendee list
/// (deduplicated, the sender's own address skipped), and - unless the sender
/// *is* the organizer or no organizer address is known - a plain notification
/// mail tells the organizer their meeting was forwarded. The local stored
/// copy is deliberately untouched: forwarding doesn't add invitees.
pub fn forward_messages(event: &CalendarEvent, identity: &OrganizerIdentity, recipients: &[EmailAddress]) -> (ComposedMessage, Option<ComposedMessage>) {
    let mut forwarded = event.clone();
    for recipient in recipients {
        let known =
            forwarded.attendees.iter().any(|attendee| attendee.address.address.eq_ignore_ascii_case(&recipient.address)) || recipient.address.eq_ignore_ascii_case(&identity.email);
        if !known {
            forwarded.attendees.push(Attendee {
                address: recipient.clone(),
                role: AttendeeRole::Required,
                status: AttendeeStatus::NeedsAction,
            });
        }
    }
    let summary = summary_or_no_subject(event);
    let to: Vec<String> = recipients.iter().map(|recipient| recipient.address.clone()).collect();
    let subject = format!("FW: {summary}");
    let text_body = format!("{} forwarded this meeting to you.", sender_label(identity));
    let forward = build_imip_message(&forwarded, ImipMethod::Request, identity, to, subject, text_body, None);

    let notification = event
        .organizer
        .as_ref()
        .filter(|organizer| !organizer.address.eq_ignore_ascii_case(&identity.email))
        .map(|organizer| {
            let body = format!(
                "\"{summary}\" was forwarded to {}.",
                recipients.iter().map(|r| r.address.as_str()).collect::<Vec<_>>().join(", ")
            );
            ComposedMessage {
                from: identity.email.clone(),
                display_name: identity.display_name.clone(),
                to: vec![organizer.address.clone()],
                cc: Vec::new(),
                bcc: Vec::new(),
                reply_to: Vec::new(),
                subject: "Your meeting was forwarded".to_string(),
                importance: lookout_core::Importance::default(),
                sensitivity: lookout_core::MessageSensitivity::Normal,
                text_body: body,
                html_body: None,
                attachments: Vec::new(),
                inline_images: Vec::new(),
                calendar_part: None,
                read_receipt: None,
                request_read_receipt: false,
                request_delivery_receipt: false,
                in_reply_to: None,
                references: Vec::new(),
                message_id: None,
            }
        });
    (forward, notification)
}

/// Applies an accepted proposal to the stored organizer event (CAL-17): the
/// proposed times replace the stored ones and `SEQUENCE` moves past the last
/// REQUEST, so the update that follows sorts after the invitation. Everything
/// else (attendees, organizer, categories, the series rule) is preserved.
pub fn apply_counter(stored: &mut CalendarEvent, proposal: &CalendarEvent) {
    stored.start = proposal.start;
    stored.end = proposal.end;
    stored.all_day = proposal.all_day;
    stored.time_zone = proposal.time_zone.clone();
    stored.sequence += 1;
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
        status: occ.status,
        reminder_minutes_before: None,
        conference_url: occ.conference_url.clone(),
        is_online_meeting: lookout_core::interop::is_teams_meeting(occ.conference_url.as_deref()),
        attachments: occ.attachments.clone(),
    }
}

/// Rebuilds the stored master event from a cached occurrence (item 3.6): the
/// inverse of the expansion for a whole-object rewrite such as applying an
/// attendee REPLY. Unlike `cancellation_event`, the series' `RRULE` and the
/// master's anchor times/href are kept - the object written back is the
/// master itself, not a withdrawn instance.
pub fn stored_event_from_occurrence(occ: &EventOccurrence) -> CalendarEvent {
    CalendarEvent {
        uid: occ.uid.clone(),
        calendar_id: occ.calendar_id.clone(),
        summary: occ.summary.clone(),
        description: occ.description.clone(),
        location: occ.location.clone(),
        start: occ.master_start.unwrap_or(occ.start),
        end: occ.master_end.unwrap_or(occ.end),
        all_day: occ.all_day,
        time_zone: occ.time_zone.clone(),
        rrule: occ.rrule.clone(),
        recurrence_id: None,
        recurrence_range: RecurrenceRange::default(),
        exdates: occ.exdates.clone(),
        rdates: Vec::new(),
        href: occ.master_href.clone().or_else(|| occ.href.clone()),
        etag: occ.master_etag.clone().or_else(|| occ.etag.clone()),
        sequence: occ.sequence,
        attendees: occ.attendees.clone(),
        organizer: occ.organizer.clone(),
        schedule_agent: occ.schedule_agent,
        categories: occ.categories.clone(),
        sensitivity: occ.sensitivity,
        show_as: occ.show_as,
        status: occ.status,
        reminder_minutes_before: occ.reminder_minutes_before,
        conference_url: occ.conference_url.clone(),
        is_online_meeting: lookout_core::interop::is_teams_meeting(occ.conference_url.as_deref()),
        attachments: occ.attachments.clone(),
    }
}

/// Merges an incoming REPLY's attendee responses onto the stored event
/// (item 3.6): only attendees the stored event already knows are updated
/// (a reply can't add invitees), matched case-insensitively by address.
/// Returns whether anything actually changed, so the caller can skip the PUT
/// when a message is rendered again.
pub fn merge_reply_statuses(stored: &mut CalendarEvent, reply: &CalendarEvent) -> bool {
    let mut changed = false;
    for incoming in &reply.attendees {
        if let Some(existing) = stored
            .attendees
            .iter_mut()
            .find(|attendee| attendee.address.address.eq_ignore_ascii_case(&incoming.address.address))
        {
            if existing.status != incoming.status {
                existing.status = incoming.status;
                changed = true;
            }
        }
    }
    changed
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
            status: lookout_core::EventStatus::default(),
            reminder_minutes_before: None,
            conference_url: None,
            is_online_meeting: false,
            attachments: Vec::new(),
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
            status: lookout_core::EventStatus::default(),
            reminder_minutes_before: None,
            conference_url: None,
            attachments: Vec::new(),
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
    fn counter_message_carries_the_proposal_to_the_organizer() {
        let identity = OrganizerIdentity {
            email: "bob@example.com".to_string(),
            display_name: Some("Bob Example".to_string()),
        };
        let organizer = EmailAddress::new("ada@contoso.com".to_string());
        let invitation = event("Planning", vec![attendee("bob@example.com")]);
        let proposed_start: chrono::DateTime<chrono::Utc> = "2026-08-10T16:00:00Z".parse().unwrap();
        let proposed_end: chrono::DateTime<chrono::Utc> = "2026-08-10T17:00:00Z".parse().unwrap();

        let counter = counter_message(
            &invitation,
            proposed_start,
            proposed_end,
            &identity,
            &organizer,
            "Monday, August 10, 2026 4:00 PM - 5:00 PM",
            Some("<invite-1@contoso.com>".to_string()),
        );
        assert_eq!(counter.from, "bob@example.com");
        assert_eq!(counter.to, vec!["ada@contoso.com".to_string()], "a counter goes to the organizer");
        assert_eq!(counter.subject, "New time proposed: Planning");
        assert!(counter.text_body.contains("Bob Example proposed a new time"), "{}", counter.text_body);
        assert!(counter.text_body.contains("4:00 PM - 5:00 PM"), "{}", counter.text_body);
        assert_eq!(counter.in_reply_to.as_deref(), Some("<invite-1@contoso.com>"));

        let ics = counter.calendar_part.as_deref().unwrap();
        assert_eq!(lookout_core::parse_imip_method(ics), ImipMethod::Counter);
        let parsed = lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), ics);
        assert_eq!(parsed[0].start, proposed_start, "the calendar document carries the proposed time");
        assert_eq!(parsed[0].end, proposed_end);
        assert_eq!(
            invitation.start,
            "2026-08-12T09:00:00Z".parse::<chrono::DateTime<chrono::Utc>>().unwrap(),
            "the original event is untouched"
        );
    }

    #[test]
    fn decline_counter_message_tells_the_proposer_the_time_stands() {
        let identity = OrganizerIdentity {
            email: "me@example.com".to_string(),
            display_name: Some("Me".to_string()),
        };
        let proposer = EmailAddress::new("bob@example.com".to_string());
        let stored = event("Planning", vec![attendee("bob@example.com")]);

        let decline = decline_counter_message(&stored, &identity, &proposer);
        assert_eq!(decline.to, vec!["bob@example.com".to_string()]);
        assert_eq!(decline.subject, "Declined: Planning");
        let ics = decline.calendar_part.as_deref().unwrap();
        assert_eq!(lookout_core::parse_imip_method(ics), ImipMethod::DeclineCounter);
        let parsed = lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), ics);
        assert_eq!(parsed[0].start, stored.start, "a declined proposal carries the original time");
    }

    #[test]
    fn forward_messages_add_the_recipient_and_notify_the_organizer() {
        let identity = OrganizerIdentity {
            email: "bob@example.com".to_string(),
            display_name: Some("Bob Example".to_string()),
        };
        let carol = EmailAddress::new("carol@example.com".to_string());
        let invitation = event("Planning", vec![attendee("bob@example.com")]);

        let (forward, notification) = forward_messages(&invitation, &identity, std::slice::from_ref(&carol));
        assert_eq!(forward.to, vec!["carol@example.com".to_string()]);
        assert_eq!(forward.subject, "FW: Planning");
        let ics = forward.calendar_part.as_deref().unwrap();
        assert_eq!(lookout_core::parse_imip_method(ics), ImipMethod::Request, "a forward is a fresh meeting request");
        let parsed = lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), ics);
        assert!(
            parsed[0].attendees.iter().any(|attendee| attendee.address.address == "carol@example.com"),
            "the new recipient rides on the calendar document"
        );
        assert_eq!(invitation.attendees.len(), 1, "the local copy isn't touched");

        let notification = notification.expect("the organizer is notified");
        assert_eq!(notification.to, vec!["me@example.com".to_string()]);
        assert_eq!(notification.subject, "Your meeting was forwarded");
        assert!(notification.calendar_part.is_none(), "the notification is plain mail");
        assert!(notification.text_body.contains("carol@example.com"), "{}", notification.text_body);
    }

    #[test]
    fn forward_from_the_organizer_skips_the_notification_and_deduplicates_recipients() {
        let identity = OrganizerIdentity {
            email: "me@example.com".to_string(),
            display_name: Some("Me".to_string()),
        };
        let alice = EmailAddress::new("alice@example.com".to_string());
        let carol = EmailAddress::new("carol@example.com".to_string());
        let meeting = event("Planning", vec![attendee("alice@example.com")]);

        let (forward, notification) = forward_messages(&meeting, &identity, &[alice.clone(), carol.clone(), alice]);
        assert!(notification.is_none(), "the organizer doesn't notify themselves");
        let ics = forward.calendar_part.as_deref().unwrap();
        let parsed = lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), ics);
        assert_eq!(
            parsed[0].attendees.iter().filter(|a| a.address.address == "alice@example.com").count(),
            1,
            "an existing attendee isn't duplicated"
        );
        assert_eq!(parsed[0].attendees.iter().filter(|a| a.address.address == "carol@example.com").count(), 1);
    }

    #[test]
    fn apply_counter_replaces_the_times_and_moves_the_sequence() {
        let mut stored = event("Planning", vec![attendee("alice@example.com"), attendee("bob@example.com")]);
        stored.sequence = 4;
        let mut proposal = stored.clone();
        proposal.start = "2026-08-14T15:00:00Z".parse().unwrap();
        proposal.end = "2026-08-14T16:00:00Z".parse().unwrap();
        proposal.time_zone = Some("Europe/Berlin".to_string());

        apply_counter(&mut stored, &proposal);
        assert_eq!(stored.start, proposal.start);
        assert_eq!(stored.end, proposal.end);
        assert_eq!(stored.time_zone.as_deref(), Some("Europe/Berlin"));
        assert_eq!(stored.sequence, 5, "the update that follows sorts after the invitation");
        assert_eq!(stored.attendees.len(), 2, "attendees are untouched");
        assert_eq!(stored.organizer.as_ref().map(|o| o.address.as_str()), Some("me@example.com"));
    }

    #[test]
    fn reply_wording_follows_the_status_and_the_send_choice() {
        assert_eq!(reply_subject(AttendeeStatus::Accepted, Some("Planning")), "Accepted: Planning");
        assert_eq!(reply_subject(AttendeeStatus::Tentative, None), "Tentatively accepted");
        assert_eq!(reply_subject(AttendeeStatus::Declined, Some("   ")), "Declined", "a blank summary falls back to the verb");
        assert_eq!(reply_text_body(AttendeeStatus::Accepted, None), "Accepted.");
        assert_eq!(reply_text_body(AttendeeStatus::Declined, Some("Can't make it")), "Can't make it\n\nDeclined.");
        assert_eq!(
            reply_text_body(AttendeeStatus::Accepted, Some("  ")),
            "Accepted.",
            "a blank comment leaves just the status line"
        );
        assert_eq!(reply_toast(AttendeeStatus::Accepted, true), "Invitation accepted");
        assert_eq!(reply_toast(AttendeeStatus::Accepted, false), "Accepted without sending a response");
    }

    #[test]
    fn reply_event_stamps_the_users_partstat_and_adds_a_missing_attendee() {
        let ics = lookout_dav::build_imip_vcalendar(&event("Planning", vec![attendee("alice@example.com")]), ImipMethod::Request);

        let replied = reply_event(&ics, "ALICE@example.com", AttendeeStatus::Accepted).expect("invitation parses");
        assert_eq!(replied.attendees.len(), 1);
        assert_eq!(replied.attendees[0].status, AttendeeStatus::Accepted, "the address match ignores case");

        let added = reply_event(&ics, "carol@example.com", AttendeeStatus::Tentative).expect("invitation parses");
        assert_eq!(added.attendees.len(), 2, "a missing recipient gets an ATTENDEE line");
        assert_eq!(added.attendees[1].address.address, "carol@example.com");
        assert_eq!(added.attendees[1].status, AttendeeStatus::Tentative);
    }

    #[test]
    fn merge_reply_statuses_updates_known_attendees_only_and_reports_changes() {
        let mut stored = event("Planning", vec![attendee("alice@example.com"), attendee("bob@example.com")]);
        let mut reply = event("Planning", vec![attendee("alice@example.com"), attendee("mallory@example.com")]);
        reply.attendees[0].status = AttendeeStatus::Accepted;
        reply.attendees[1].status = AttendeeStatus::Accepted;

        assert!(merge_reply_statuses(&mut stored, &reply));
        assert_eq!(stored.attendees[0].status, AttendeeStatus::Accepted);
        assert_eq!(stored.attendees[1].status, AttendeeStatus::NeedsAction, "untouched attendees keep their status");
        assert_eq!(stored.attendees.len(), 2, "a reply never adds invitees");
        assert!(!merge_reply_statuses(&mut stored, &reply), "an already-applied reply reports no change");
    }

    #[test]
    fn stored_event_from_occurrence_keeps_the_master_anchor_and_rule() {
        let mut occ = occurrence(vec![attendee("alice@example.com")], Some("FREQ=WEEKLY"));
        occ.master_start = Some("2026-08-01T09:00:00Z".parse().unwrap());
        occ.master_end = Some("2026-08-01T10:00:00Z".parse().unwrap());
        occ.master_href = Some("/cal/master.ics".to_string());
        occ.master_etag = Some("\"v2\"".to_string());

        let stored = stored_event_from_occurrence(&occ);
        assert_eq!(stored.rrule.as_deref(), Some("FREQ=WEEKLY"));
        assert!(stored.recurrence_id.is_none(), "the master object is what gets written back");
        assert_eq!(stored.href.as_deref(), Some("/cal/master.ics"));
        assert_eq!(stored.etag.as_deref(), Some("\"v2\""));
        assert_eq!(stored.start, occ.master_start.unwrap(), "the expanded instance time is not written to the master");
        assert_eq!(stored.sequence, occ.sequence);
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
