//! iMIP meeting invitations: organizer requests/cancels, invite-card rendering, and the RSVP, counter-proposal and forward flows.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use chrono::Timelike;
use gtk::{gio, glib};
use lookout_core::calendar_protocol::CalendarCommand;
use lookout_core::{AccountId, AttendeeStatus, CalendarEvent, CalendarId, EmailAddress, EventOccurrence, EventUid, MailboxId, Uid};
use lookout_mail::session::AccountCommand;

use crate::worker::Worker;

use super::*;

/// The mail identity that organizes `calendar_id`'s events: the calendar
/// account's GOA id is the same D-Bus object path its Mail interface carries
/// (see `lookout-goa`'s discovery), so the mail handle with that id is the
/// same account and its address is the `From`/`ORGANIZER`. Falls back to the
/// display-name heuristic (`calendar_owner_email`) matched against a mail
/// account's address for calendar-only sessions. `None` means "no mail
/// account to send from" - the event still saves, just without invitations.
pub(crate) fn invite_identity(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    calendar_id: &CalendarId,
) -> Option<(crate::calendar_invites::OrganizerIdentity, async_channel::Sender<AccountCommand>)> {
    let (account_id, display_name) = calendar_state.borrow().accounts.iter().find_map(|(id, handle)| {
        handle
            .calendars
            .iter()
            .any(|calendar| calendar.id == *calendar_id)
            .then(|| (id.clone(), handle.display_name.clone()))
    })?;
    let state = state.borrow();
    if let Some(handle) = state.accounts.get(&account_id) {
        return Some((organizer_identity(handle), handle.cmd_tx.clone()));
    }
    let candidate = crate::recipient_entry::is_plausible_address(&display_name).then_some(display_name)?;
    state
        .accounts
        .values()
        .find(|handle| handle.email.eq_ignore_ascii_case(&candidate))
        .map(|handle| (organizer_identity(handle), handle.cmd_tx.clone()))
}

pub(crate) fn organizer_identity(handle: &AccountHandle) -> crate::calendar_invites::OrganizerIdentity {
    crate::calendar_invites::OrganizerIdentity {
        email: handle.email.clone(),
        display_name: (!handle.display_name.trim().is_empty()).then(|| handle.display_name.clone()),
    }
}

/// Presents an Outlook-style "send / don't send" prompt and runs `on_accept`
/// on the affirmative answer; the close/Escape response declines. The dialog
/// is presented via the main loop, so the caller's own action (the save or
/// delete) has already been dispatched by the time the user answers.
pub(crate) fn prompt_invite(window: &adw::ApplicationWindow, heading: &str, body: &str, accept_label: &str, on_accept: impl FnOnce() + 'static) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .default_response("send")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Don't send");
    dialog.add_response("send", accept_label);
    dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
    let (respond_tx, respond_rx) = async_channel::bounded(1);
    dialog.connect_response(None, move |_dialog, response| {
        let _ = respond_tx.send_blocking(response == "send");
    });
    dialog.present(Some(window));
    glib::spawn_future_local(async move {
        if matches!(respond_rx.recv().await, Ok(true)) {
            on_accept();
        }
    });
}

/// Sends one iMIP message through the organizer's mail account's outbox and
/// toasts the outcome. A message with no recipients left after removing the
/// organizer's own address is dropped silently (that's an event with only the
/// organizer on its attendee list, not a failure).
pub(crate) fn send_invite(
    cmd_tx: &async_channel::Sender<AccountCommand>,
    identity: &crate::calendar_invites::OrganizerIdentity,
    event: &CalendarEvent,
    kind: crate::calendar_invites::InviteKind,
    toast_overlay: &adw::ToastOverlay,
    toast: &str,
) {
    let message = crate::calendar_invites::invite_message(event, kind, identity);
    if message.to.is_empty() {
        return;
    }
    let _ = cmd_tx.send_blocking(AccountCommand::SendMessage {
        message: Box::new(message),
        not_before: None,
        client_token: None,
    });
    toast_overlay.add_toast(adw::Toast::new(toast));
}

pub(crate) fn no_invite_sender_toast(toast_overlay: &adw::ToastOverlay, action: &str) {
    toast_overlay.add_toast(adw::Toast::new(&format!("{action} - no connected mail account matches this calendar.")));
}

/// The organizer-aware save path (OUTLOOK_PARITY item 3.4 / CAL-14): bumps
/// `SEQUENCE` on edits, routes the PUT exactly as `route_calendar_save` does,
/// then either sends the invitation immediately (a new meeting), prompts for
/// an update, or prompts to cancel removed attendees - except on a server
/// that schedules itself (RFC 6638), where saving is the whole job. Used by
/// the event editor and the drag-reschedule gesture; the `.ics` import and
/// the incoming-invitation upsert deliberately use the plain route, since
/// neither is an organizer action.
pub(crate) fn save_event_with_invites(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    previous: Option<&EventOccurrence>,
    calendar_id: CalendarId,
    mut event: CalendarEvent,
) {
    // RFC 5545 §3.8.1.4: a revision the attendees see must move SEQUENCE
    // forward. Bumped whether the client or the server delivers, so a
    // scheduling server re-schedules too.
    if let Some(previous) = previous {
        if !event.attendees.is_empty() || !previous.attendees.is_empty() {
            event.sequence = previous.sequence + 1;
        }
        // Preserve another scheduling client's per-event override.
        if event.schedule_agent.is_none() {
            event.schedule_agent = previous.schedule_agent;
        }
    }
    // The organizer identity is resolved before the PUT so a meeting whose
    // stored ORGANIZER is missing (older events, or an account whose address
    // only became known now) is stamped with the address we'd send from.
    let identity = invite_identity(state, calendar_state, &calendar_id);
    if event.organizer.is_none() && !event.attendees.is_empty() {
        if let Some((identity, _)) = &identity {
            event.organizer = Some(EmailAddress::new(identity.email.clone()));
        }
    }
    let delivery = crate::calendar_invites::delivery_for(&calendar_scheduling(calendar_state, &calendar_id), event.schedule_agent);
    let action = if delivery == crate::calendar_invites::Delivery::Client {
        crate::calendar_invites::classify_save(previous, &event)
    } else {
        crate::calendar_invites::SaveInvite::None
    };
    route_calendar_save(calendar_state, calendar_id, event.clone());

    let summary = event.summary.clone().unwrap_or_default();
    let Some(action) = (action != crate::calendar_invites::SaveInvite::None).then_some(action) else {
        return;
    };
    let Some((identity, cmd_tx)) = identity else {
        let action = match action {
            crate::calendar_invites::SaveInvite::Request => "Invitations not sent",
            crate::calendar_invites::SaveInvite::PromptCancel => "Cancellation not sent",
            _ => "Update not sent",
        };
        no_invite_sender_toast(toast_overlay, action);
        return;
    };
    // A meeting organized by someone else (an invitation stored from mail):
    // saving our copy must never mail its attendees on the organizer's
    // behalf. The cancellation path checks the pre-edit state, since removing
    // every attendee also clears the ORGANIZER.
    let organizer_matches = match action {
        crate::calendar_invites::SaveInvite::PromptCancel => previous.map(|previous| previous.organizer.as_ref()).unwrap_or(event.organizer.as_ref()),
        _ => event.organizer.as_ref(),
    };
    if !crate::calendar_invites::is_organizer(organizer_matches, &identity.email) {
        return;
    }
    match action {
        crate::calendar_invites::SaveInvite::None => {}
        crate::calendar_invites::SaveInvite::Request => {
            send_invite(&cmd_tx, &identity, &event, crate::calendar_invites::InviteKind::Request, toast_overlay, "Invitation sent");
        }
        crate::calendar_invites::SaveInvite::PromptUpdate => {
            let toast_overlay = toast_overlay.clone();
            let body = format!("\"{summary}\" was updated. Send the attendees an update?");
            prompt_invite(window, "Send update to attendees?", &body, "Send update", move || {
                send_invite(&cmd_tx, &identity, &event, crate::calendar_invites::InviteKind::Update, &toast_overlay, "Update sent");
            });
        }
        crate::calendar_invites::SaveInvite::PromptCancel => {
            let Some(previous) = previous.cloned() else { return };
            let toast_overlay = toast_overlay.clone();
            let mut cancel_event = crate::calendar_invites::cancellation_event(&previous, Some(EmailAddress::new(identity.email.clone())));
            // The cancellation must sort after the last REQUEST, which
            // carried the pre-edit sequence.
            cancel_event.sequence = previous.sequence + 1;
            let body = format!("\"{summary}\" has no attendees left. Cancel with the people who were removed?");
            prompt_invite(window, "Send cancellation to removed attendees?", &body, "Send cancellation", move || {
                send_invite(
                    &cmd_tx,
                    &identity,
                    &cancel_event,
                    crate::calendar_invites::InviteKind::Cancel,
                    &toast_overlay,
                    "Cancellation sent",
                );
            });
        }
    }
}

/// The organizer-aware delete path: routes the deletion exactly as
/// `route_calendar_delete` does (recurring instances become EXDATEs, overrides
/// and standalone events lose their resource), then prompts to send a
/// cancellation to the attendees - the delete itself always happens, matching
/// Outlook, so "Don't send" leaves a silent removal. Server-scheduled
/// accounts skip the prompt.
pub(crate) fn delete_event_with_invites(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    calendar_id: CalendarId,
    occ: EventOccurrence,
) {
    let delivery = crate::calendar_invites::delivery_for(&calendar_scheduling(calendar_state, &calendar_id), occ.schedule_agent);
    let action = crate::calendar_invites::classify_delete(&occ, delivery);
    route_calendar_delete(calendar_state, calendar_id, occ.clone());
    if action != crate::calendar_invites::DeleteInvite::PromptCancel {
        return;
    }
    match invite_identity(state, calendar_state, &occ.calendar_id) {
        Some((identity, cmd_tx)) => {
            // An event organized by someone else (stored from an incoming
            // invitation): deleting our copy must not cancel it for its real
            // organizer's attendees.
            if !crate::calendar_invites::is_organizer(occ.organizer.as_ref(), &identity.email) {
                return;
            }
            let toast_overlay = toast_overlay.clone();
            let summary = occ.summary.clone().unwrap_or_default();
            let cancel_event = crate::calendar_invites::cancellation_event(&occ, Some(EmailAddress::new(identity.email.clone())));
            let body = format!("\"{summary}\" was deleted. Notify the attendees with a cancellation?");
            prompt_invite(window, "Send cancellation to attendees?", &body, "Send cancellation", move || {
                send_invite(
                    &cmd_tx,
                    &identity,
                    &cancel_event,
                    crate::calendar_invites::InviteKind::Cancel,
                    &toast_overlay,
                    "Cancellation sent",
                );
            });
        }
        None => no_invite_sender_toast(toast_overlay, "Cancellation not sent"),
    }
}

/// The calendar object already stored under `event.uid`, if any - the iMIP
/// reply flows upsert against it instead of creating a duplicate.
pub(crate) fn find_calendar_occurrence(calendar_state: &Rc<RefCell<CalendarUiState>>, uid: &EventUid) -> Option<EventOccurrence> {
    let st = calendar_state.borrow();
    let occurrences = st
        .accounts
        .values()
        .flat_map(|handle| handle.last_occurrences.iter())
        .filter(|occurrence| occurrence.uid == *uid)
        .collect::<Vec<_>>();
    // Overrides share their master's UID; prefer a master (or any
    // non-recurring) occurrence so whole-event lookups (the iMIP upsert
    // path) don't accidentally resolve to a single-instance override.
    occurrences
        .iter()
        .find(|occ| occ.recurrence_id.is_none())
        .or_else(|| occurrences.first())
        .map(|occ| (*occ).clone())
}

/// The user's answer to an iMIP invitation (REQUEST): sends the RFC 6047
/// `METHOD:REPLY` email back to the organizer with the chosen `PARTSTAT` and
/// an optional comment (CAL-15), and best-effort saves the event into the
/// account's default calendar, upserting by UID so accepting a re-invitation
/// updates the existing booking rather than duplicating it. With
/// `send_response: false` (the card's "Don't send a response") the calendar
/// answer still applies but no mail goes out. The reply is the primary
/// action: a missing or busy calendar setup is a toast, not a blocked send.
#[allow(clippy::too_many_arguments)]
pub(crate) fn respond_to_imip_invitation(
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    invitation: &lookout_core::ImipInvitation,
    from_email: &str,
    display_name: Option<&str>,
    cmd_tx: &async_channel::Sender<AccountCommand>,
    status: AttendeeStatus,
    send_response: bool,
    comment: Option<&str>,
) {
    let Some(event) = crate::calendar_invites::reply_event(&invitation.ics, from_email, status) else {
        toast_overlay.add_toast(adw::Toast::new("Couldn't parse this invitation's calendar data."));
        return;
    };
    // On a Microsoft 365 (Graph) calendar the server owns iTIP delivery, so
    // respond through Graph's RSVP action rather than sending an iMIP REPLY
    // (item 13). The invited event must be in a synced month for its Graph id
    // to be known; otherwise this falls through to the iMIP path.
    if let Some(occ) = find_calendar_occurrence(calendar_state, &event.uid) {
        if calendar_account_is_graph(calendar_state, &occ.calendar_id) {
            let response = match status {
                AttendeeStatus::Accepted => lookout_core::calendar_protocol::InvitationResponse::Accept,
                AttendeeStatus::Tentative => lookout_core::calendar_protocol::InvitationResponse::Tentative,
                AttendeeStatus::Declined => lookout_core::calendar_protocol::InvitationResponse::Decline,
                AttendeeStatus::NeedsAction => return,
            };
            if let Some(href) = occ.href.clone() {
                if let Some(handle) = calendar_handle_for_id(calendar_state, &occ.calendar_id) {
                    let _ = handle.send_blocking(CalendarCommand::RespondInvitation {
                        calendar_id: occ.calendar_id.clone(),
                        event_id: href,
                        response,
                        comment: comment.map(str::to_string),
                        send_response,
                        proposed_new_time: None,
                    });
                }
            }
            toast_overlay.add_toast(adw::Toast::new(crate::calendar_invites::reply_toast(status, send_response)));
            return;
        }
    }
    if send_response {
        let Some(organizer) = &invitation.organizer else {
            toast_overlay.add_toast(adw::Toast::new("This invitation names no organizer - can't send a reply."));
            return;
        };
        let identity = crate::calendar_invites::OrganizerIdentity {
            email: from_email.to_string(),
            display_name: display_name.map(str::to_string).filter(|name| !name.trim().is_empty()),
        };
        let message = crate::calendar_invites::build_imip_message(
            &event,
            lookout_core::ImipMethod::Reply,
            &identity,
            vec![organizer.address.clone()],
            crate::calendar_invites::reply_subject(status, invitation.summary.as_deref()),
            crate::calendar_invites::reply_text_body(status, comment),
            invitation.in_reply_to.clone(),
        );
        let _ = cmd_tx.send_blocking(AccountCommand::SendMessage {
            message: Box::new(message),
            not_before: None,
            client_token: None,
        });
    }
    toast_overlay.add_toast(adw::Toast::new(crate::calendar_invites::reply_toast(status, send_response)));
    save_replied_event(calendar_state, toast_overlay, event);
}

/// Best-effort save of a responded-to invitation: writes over the stored
/// object when one exists under the same UID, otherwise creates it in the
/// default calendar. The reply mail (when one was sent) already went out, so
/// a missing or busy calendar is a warning, never a blocker.
pub(crate) fn save_replied_event(calendar_state: &Rc<RefCell<CalendarUiState>>, toast_overlay: &adw::ToastOverlay, mut event: CalendarEvent) {
    let Some(default_calendar) = default_pickable_calendar(calendar_state) else {
        toast_overlay.add_toast(adw::Toast::new("Connect a calendar account to save the event."));
        return;
    };
    match find_calendar_occurrence(calendar_state, &event.uid) {
        // A re-invitation updating an event already in the calendar: write
        // over the stored object in place.
        Some(existing) => {
            event.calendar_id = existing.calendar_id.clone();
            event.href = existing.href;
            event.etag = existing.etag;
            let handle = calendar_handle_for_id(calendar_state, &event.calendar_id);
            let _ = handle.map(|handle| handle.send_blocking(CalendarCommand::UpdateEvent { event: Box::new(event) }));
        }
        None => {
            event.calendar_id = default_calendar;
            route_calendar_save(calendar_state, event.calendar_id.clone(), event);
        }
    }
}

/// Everything a card response action needs from the rendered message: the
/// parsed invitation, the receiving identity and its mail session, and the
/// message key used to retire the card/banner once answered.
pub(crate) struct ImipReplyContext {
    pub(crate) invitation: lookout_core::ImipInvitation,
    pub(crate) from_email: String,
    pub(crate) display_name: String,
    pub(crate) cmd_tx: async_channel::Sender<AccountCommand>,
    pub(crate) account_id: AccountId,
    pub(crate) mailbox: MailboxId,
    pub(crate) uid: Uid,
}

/// Resolves the rendered message's iMIP context at the moment a card action
/// fires - the same per-message state the banner handler reads on click.
pub(crate) fn imip_reply_context(state: &Rc<RefCell<UiState>>) -> Option<ImipReplyContext> {
    let st = state.borrow();
    let (mailbox, uid) = st.rendered_message.clone()?;
    let invitation = st.imip.clone()?;
    let account_id = mailbox_account_id(&mailbox)?;
    let handle = st.accounts.get(&account_id)?;
    Some(ImipReplyContext {
        invitation,
        from_email: handle.email.clone(),
        display_name: handle.display_name.clone(),
        cmd_tx: handle.cmd_tx.clone(),
        account_id,
        mailbox,
        uid,
    })
}

/// Retires the invitation surfaces for one message: the banner and card hide
/// now, and the message stays dismissed across re-renders, exactly as when
/// the banner's response dialog was answered.
pub(crate) fn dismiss_imip(state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, mailbox: &MailboxId, uid: Uid) {
    state.borrow_mut().imip_dismissed = Some((mailbox.clone(), uid));
    if let Some(banner) = find_named_child(reading_stack, "imip-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
        banner.set_revealed(false);
    }
    render_invite_card(reading_stack, None, &[], None);
}

/// Applies an incoming iMIP REPLY to the stored organizer event (item 3.6 /
/// CAL-16): the replying attendees' `PARTSTAT`s are merged onto the calendar
/// object and PUT back. Only an organizer's own copy is updated, and only
/// when this client owns iTIP delivery - on an RFC 6638-scheduled account the
/// server applies responses itself (the mail is informational there). Runs
/// when a REPLY message is rendered (its body is only fetched when opened),
/// and `UiState::imip_replies_applied` keeps a re-render from writing twice.
pub(crate) fn apply_incoming_imip_reply(state: &Rc<RefCell<UiState>>, calendar_state: &Rc<RefCell<CalendarUiState>>, invitation: &lookout_core::ImipInvitation) {
    let Some(reply_event) = lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), &invitation.ics).into_iter().next() else {
        return;
    };
    let Some(existing) = find_calendar_occurrence(calendar_state, &reply_event.uid) else {
        return;
    };
    // A reply only changes the organizer's copy.
    let Some((identity, _)) = invite_identity(state, calendar_state, &existing.calendar_id) else {
        return;
    };
    if !crate::calendar_invites::is_organizer(existing.organizer.as_ref(), &identity.email) {
        return;
    }
    // With server-side scheduling the calendar server applies replies itself
    // (RFC 6638); writing the same change from here would race it.
    let scheduling = calendar_state
        .borrow()
        .accounts
        .values()
        .find(|handle| handle.calendars.iter().any(|calendar| calendar.id == existing.calendar_id))
        .map(|handle| handle.scheduling.clone());
    if scheduling.is_some_and(|scheduling| crate::calendar_invites::delivery_for(&scheduling, existing.schedule_agent) == crate::calendar_invites::Delivery::Server) {
        return;
    }
    // Rebuild the master and merge just the PARTSTATs; a reply that repeats
    // what the stored event already says changes nothing and skips the PUT.
    let mut stored = crate::calendar_invites::stored_event_from_occurrence(&existing);
    if !crate::calendar_invites::merge_reply_statuses(&mut stored, &reply_event) {
        return;
    }
    let handle = calendar_handle_for_id(calendar_state, &stored.calendar_id);
    let _ = handle.map(|handle| handle.send_blocking(CalendarCommand::UpdateEvent { event: Box::new(stored) }));
}

/// The card actions' stable string parameter for one answer.
pub(crate) fn response_status_param(status: AttendeeStatus) -> &'static str {
    match status {
        AttendeeStatus::Accepted => "accepted",
        AttendeeStatus::Tentative => "tentative",
        AttendeeStatus::Declined => "declined",
        AttendeeStatus::NeedsAction => "needs-action",
    }
}

pub(crate) fn response_status_from_param(param: &str) -> Option<AttendeeStatus> {
    match param {
        "accepted" => Some(AttendeeStatus::Accepted),
        "tentative" => Some(AttendeeStatus::Tentative),
        "declined" => Some(AttendeeStatus::Declined),
        _ => None,
    }
}

/// Opens the full-window composer for an invitation response (CAL-15): prefilled
/// with the organizer, the status line and the RFC 6047 REPLY's calendar part,
/// so the user can add a comment before sending. The local calendar answer is
/// saved when Send fires (see `ImipReplySend`), not when the composer opens -
/// cancelling the composer must leave the calendar untouched.
pub(crate) fn open_imip_reply_composer(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
    context: &ImipReplyContext,
    status: AttendeeStatus,
) {
    let Some(organizer) = context.invitation.organizer.clone() else {
        toast_overlay.add_toast(adw::Toast::new("This invitation names no organizer - can't send a reply."));
        return;
    };
    let Some(event) = crate::calendar_invites::reply_event(&context.invitation.ics, &context.from_email, status) else {
        toast_overlay.add_toast(adw::Toast::new("Couldn't parse this invitation's calendar data."));
        return;
    };
    let recipient = match organizer.name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("{name} <{}>", organizer.address),
        None => organizer.address.clone(),
    };
    let prefill = crate::compose::ComposePrefill {
        to: Some(recipient),
        subject: Some(crate::calendar_invites::reply_subject(status, context.invitation.summary.as_deref())),
        body: Some(crate::calendar_invites::reply_text_body(status, None)),
        calendar_part: Some(lookout_dav::build_imip_vcalendar(&event, lookout_core::ImipMethod::Reply)),
        in_reply_to: context.invitation.in_reply_to.clone(),
        ..Default::default()
    };
    let on_applied = {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let mailbox = context.mailbox.clone();
        let uid = context.uid;
        Rc::new(move || dismiss_imip(&state, &reading_stack, &mailbox, uid))
    };
    show_composer_in_reading_pane_full(
        state,
        worker,
        reading_stack,
        "Reply to invitation",
        context.cmd_tx.clone(),
        prefill,
        // The calendar part replaces any HTML at send time, so plain text is
        // the one body the user can actually verify before sending.
        false,
        Some(context.account_id.clone()),
        None,
        Some(ImipReplySend {
            calendar_state: calendar_state.clone(),
            toast_overlay: toast_overlay.clone(),
            event,
            on_applied,
        }),
        true,
    );
}

/// The rendered message's account as the iMIP sender identity.
pub(crate) fn imip_identity(context: &ImipReplyContext) -> crate::calendar_invites::OrganizerIdentity {
    crate::calendar_invites::OrganizerIdentity {
        email: context.from_email.clone(),
        display_name: (!context.display_name.trim().is_empty()).then(|| context.display_name.clone()),
    }
}

/// Parses a rendered invitation's calendar payload into its event (the
/// invitation, the proposal or the forwarded copy - the method lives in the
/// document's `METHOD`, not the VEVENT).
pub(crate) fn imip_event(invitation: &lookout_core::ImipInvitation) -> Option<CalendarEvent> {
    lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), &invitation.ics).into_iter().next()
}

/// Applies an attendee's accepted time proposal (CAL-17): the stored
/// organizer event takes the proposed time (`SEQUENCE++` via `apply_counter`)
/// and is PUT back, and - where this client owns iTIP delivery - the update
/// goes out to every attendee straight away, as Outlook does. An RFC 6638
/// server-scheduled account just PUTs; the server schedules the update
/// itself. Returns whether the proposal was applied (so the card retires).
pub(crate) fn accept_counter_proposal(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    invitation: &lookout_core::ImipInvitation,
) -> bool {
    let Some(proposal) = imip_event(invitation) else {
        toast_overlay.add_toast(adw::Toast::new("Couldn't parse this proposal's calendar data."));
        return false;
    };
    let Some(existing) = find_calendar_occurrence(calendar_state, &proposal.uid) else {
        toast_overlay.add_toast(adw::Toast::new("This event isn't in your calendar."));
        return false;
    };
    let identity = invite_identity(state, calendar_state, &existing.calendar_id);
    if let Some((identity, _)) = &identity {
        // Only the organizer may accept: a counter that reached someone else
        // must not rewrite the meeting's times.
        if !crate::calendar_invites::is_organizer(existing.organizer.as_ref(), &identity.email) {
            toast_overlay.add_toast(adw::Toast::new("Only the organizer can accept a time proposal."));
            return false;
        }
    }
    let mut stored = crate::calendar_invites::stored_event_from_occurrence(&existing);
    crate::calendar_invites::apply_counter(&mut stored, &proposal);
    let delivery = crate::calendar_invites::delivery_for(&calendar_scheduling(calendar_state, &stored.calendar_id), stored.schedule_agent);
    let Some(handle) = calendar_handle_for_id(calendar_state, &stored.calendar_id) else {
        toast_overlay.add_toast(adw::Toast::new("Couldn't update the event in your calendar."));
        return false;
    };
    let _ = handle.send_blocking(CalendarCommand::UpdateEvent { event: Box::new(stored.clone()) });
    match (delivery, identity) {
        (crate::calendar_invites::Delivery::Client, Some((identity, cmd_tx))) => {
            send_invite(&cmd_tx, &identity, &stored, crate::calendar_invites::InviteKind::Update, toast_overlay, "New time accepted");
        }
        (crate::calendar_invites::Delivery::Client, None) => {
            toast_overlay.add_toast(adw::Toast::new("New time accepted"));
            no_invite_sender_toast(toast_overlay, "Update not sent");
        }
        (crate::calendar_invites::Delivery::Server, _) => {
            toast_overlay.add_toast(adw::Toast::new("New time accepted"));
        }
    }
    true
}

/// Turns down an attendee's time proposal (CAL-17): a `DECLINECOUNTER`
/// carries the originally scheduled times back to the proposing attendee.
/// The calendar itself is untouched - the meeting stays where it was.
pub(crate) fn decline_counter_proposal(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    invitation: &lookout_core::ImipInvitation,
) -> bool {
    let Some(proposal) = imip_event(invitation) else {
        toast_overlay.add_toast(adw::Toast::new("Couldn't parse this proposal's calendar data."));
        return false;
    };
    let Some(proposer) = invitation.proposer.clone() else {
        toast_overlay.add_toast(adw::Toast::new("This proposal names no attendee to answer."));
        return false;
    };
    let Some(existing) = find_calendar_occurrence(calendar_state, &proposal.uid) else {
        toast_overlay.add_toast(adw::Toast::new("This event isn't in your calendar."));
        return false;
    };
    let Some((identity, cmd_tx)) = invite_identity(state, calendar_state, &existing.calendar_id) else {
        no_invite_sender_toast(toast_overlay, "Proposal not declined");
        return false;
    };
    if !crate::calendar_invites::is_organizer(existing.organizer.as_ref(), &identity.email) {
        toast_overlay.add_toast(adw::Toast::new("Only the organizer can decline a time proposal."));
        return false;
    }
    let stored = crate::calendar_invites::stored_event_from_occurrence(&existing);
    let message = crate::calendar_invites::decline_counter_message(&stored, &identity, &proposer);
    let _ = cmd_tx.send_blocking(AccountCommand::SendMessage {
        message: Box::new(message),
        not_before: None,
        client_token: None,
    });
    toast_overlay.add_toast(adw::Toast::new("Proposal declined"));
    true
}

/// A proposed slot as the plain-text proposal body names it ("Monday, August
/// 10, 2026 4:00 PM - 5:00 PM"), rendered in the calendar's display zone.
pub(crate) fn format_proposal_when(start: chrono::DateTime<chrono::Utc>, end: chrono::DateTime<chrono::Utc>) -> String {
    let (start_local, end_local) = crate::calendar_tz::occurrence_span(crate::calendar_tz::display_zone(), start, end, false);
    format!(
        "{} {} - {}",
        start_local.format("%A, %B %-d, %Y"),
        start_local.format("%-I:%M %p"),
        end_local.format("%-I:%M %p")
    )
}

/// A proposed slot - the start and end instants in UTC - as the time-proposal
/// dialog produces it.
pub(crate) type ProposalSpan = (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>);

/// The attendee's "Propose new time" dialog (CAL-17): a date plus start/end
/// times prefilled from the invited slot, accepted only while the span is
/// valid. Sending builds the `METHOD:COUNTER` message (see
/// `calendar_invites::counter_message`) and puts it in the outbox; the
/// invitation card stays put - the meeting isn't in the calendar yet, and the
/// organizer's update will settle it.
pub(crate) fn show_propose_new_time_dialog(parent: &gtk::Widget, toast_overlay: &adw::ToastOverlay, context: &ImipReplyContext) {
    let Some(organizer) = context.invitation.organizer.clone() else {
        toast_overlay.add_toast(adw::Toast::new("This invitation names no organizer - can't propose a time."));
        return;
    };
    let zone = crate::calendar_tz::display_zone();
    let (invited_start, invited_end) = crate::calendar_tz::occurrence_span(zone, context.invitation.start, context.invitation.end, context.invitation.all_day);
    // An all-day invitation has no wall-clock slots to prefill from; suggest
    // a morning hour rather than midnight.
    let (prefill_start, prefill_end) = if context.invitation.all_day {
        let date = invited_start.date();
        (date.and_hms_opt(9, 0, 0).unwrap(), date.and_hms_opt(10, 0, 0).unwrap())
    } else {
        (invited_start, invited_end)
    };

    let date = Rc::new(Cell::new(prefill_start.date()));
    let calendar = gtk::Calendar::new();
    crate::datetime_picker::set_calendar_date(&calendar, &date, prefill_start.date());
    crate::datetime_picker::wire_calendar(&calendar, &date);
    let start_hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
    let start_minute = gtk::SpinButton::with_range(0.0, 59.0, 1.0);
    let end_hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
    let end_minute = gtk::SpinButton::with_range(0.0, 59.0, 1.0);
    for spin in [&start_hour, &start_minute, &end_hour, &end_minute] {
        spin.set_digits(0);
        spin.set_width_chars(2);
    }
    start_hour.set_value(f64::from(prefill_start.hour()));
    start_minute.set_value(f64::from(prefill_start.minute()));
    end_hour.set_value(f64::from(prefill_end.hour()));
    end_minute.set_value(f64::from(prefill_end.minute()));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.append(&crate::datetime_picker::time_cluster("New time", &calendar, &start_hour, &start_minute));
    let end_cluster = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    let ends_label = gtk::Label::builder().label("Ends").css_classes(["dim-label", "caption"]).xalign(0.0).build();
    crate::privacy::mark_ui(&ends_label);
    end_cluster.append(&ends_label);
    let end_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).build();
    end_row.append(&end_hour);
    end_row.append(&gtk::Label::builder().label(":").css_classes(["dim-label"]).build());
    end_row.append(&end_minute);
    end_cluster.append(&end_row);
    content.append(&end_cluster);

    let dialog = adw::AlertDialog::builder()
        .heading("Propose new time")
        .body(format!(
            "Propose a different time for \"{}\" to the organizer.",
            context.invitation.summary.as_deref().unwrap_or("this event")
        ))
        .default_response("propose")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("propose", "Propose time");
    dialog.set_response_appearance("propose", adw::ResponseAppearance::Suggested);
    dialog.set_extra_child(Some(&content));

    let chosen: Rc<dyn Fn() -> Option<ProposalSpan>> = {
        let date = date.clone();
        let start_hour = start_hour.clone();
        let start_minute = start_minute.clone();
        let end_hour = end_hour.clone();
        let end_minute = end_minute.clone();
        Rc::new(move || {
            let start_time = chrono::NaiveTime::from_hms_opt(start_hour.value() as u32, start_minute.value() as u32, 0)?;
            let end_time = chrono::NaiveTime::from_hms_opt(end_hour.value() as u32, end_minute.value() as u32, 0)?;
            let start_naive = date.get().and_time(start_time);
            let end_naive = date.get().and_time(end_time);
            (end_naive > start_naive).then(|| (zone.resolve_local(start_naive), zone.resolve_local(end_naive)))
        })
    };
    let refresh: Rc<dyn Fn()> = {
        let dialog = dialog.clone();
        let chosen = chosen.clone();
        Rc::new(move || dialog.set_response_enabled("propose", chosen().is_some()))
    };
    refresh();
    {
        let refresh = refresh.clone();
        calendar.connect_day_selected(move |_| refresh());
    }
    for spin in [&start_hour, &start_minute, &end_hour, &end_minute] {
        let refresh = refresh.clone();
        spin.connect_value_changed(move |_| refresh());
    }
    {
        let invitation = context.invitation.clone();
        let identity = imip_identity(context);
        let cmd_tx = context.cmd_tx.clone();
        let toast_overlay = toast_overlay.clone();
        let chosen = chosen.clone();
        dialog.connect_response(None, move |_dialog, response| {
            if response != "propose" {
                return;
            }
            let Some((start, end)) = chosen() else { return };
            let Some(event) = imip_event(&invitation) else {
                toast_overlay.add_toast(adw::Toast::new("Couldn't parse this invitation's calendar data."));
                return;
            };
            let message = crate::calendar_invites::counter_message(&event, start, end, &identity, &organizer, &format_proposal_when(start, end), invitation.in_reply_to.clone());
            let _ = cmd_tx.send_blocking(AccountCommand::SendMessage {
                message: Box::new(message),
                not_before: None,
                client_token: None,
            });
            toast_overlay.add_toast(adw::Toast::new("New time proposed"));
        });
    }
    dialog.present(Some(parent));
}

/// The meeting-forward dialog (CAL-18): recipient chips, then a fresh
/// `METHOD:REQUEST` to them and - unless the sender is the organizer - a
/// plain notification to the organizer. The local copy is untouched; the
/// forward is a mail, not an edit.
pub(crate) fn show_forward_meeting_dialog(
    parent: &gtk::Widget,
    toast_overlay: &adw::ToastOverlay,
    identity: &crate::calendar_invites::OrganizerIdentity,
    cmd_tx: &async_channel::Sender<AccountCommand>,
    event: &CalendarEvent,
) {
    let to_field = crate::recipient_entry::RecipientEntry::new("To");
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.append(to_field.widget());

    let dialog = adw::AlertDialog::builder()
        .heading("Forward meeting")
        .body(format!(
            "Forward \"{}\" to new recipients. The organizer is told the meeting was forwarded.",
            event.summary.as_deref().unwrap_or("this meeting")
        ))
        .default_response("forward")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("forward", "Forward");
    dialog.set_response_appearance("forward", adw::ResponseAppearance::Suggested);
    dialog.set_extra_child(Some(&content));
    dialog.set_response_enabled("forward", false);
    {
        let dialog = dialog.clone();
        let field = to_field.clone();
        to_field.connect_addresses_changed(move || {
            dialog.set_response_enabled("forward", !field.addresses().is_empty());
        });
    }
    {
        let to_field = to_field.clone();
        let identity = identity.clone();
        let cmd_tx = cmd_tx.clone();
        let event = event.clone();
        let toast_overlay = toast_overlay.clone();
        dialog.connect_response(None, move |_dialog, response| {
            if response != "forward" {
                return;
            }
            to_field.commit_pending();
            let recipients: Vec<EmailAddress> = to_field
                .addresses()
                .iter()
                .filter(|token| crate::recipient_entry::is_plausible_address(token))
                .map(|token| EmailAddress::new(crate::recipient_entry::address_of(token).to_string()))
                .collect();
            if recipients.is_empty() {
                toast_overlay.add_toast(adw::Toast::new("Enter at least one valid address to forward the meeting."));
                return;
            }
            let (forward, notification) = crate::calendar_invites::forward_messages(&event, &identity, &recipients);
            let _ = cmd_tx.send_blocking(AccountCommand::SendMessage {
                message: Box::new(forward),
                not_before: None,
                client_token: None,
            });
            if let Some(notification) = notification {
                let _ = cmd_tx.send_blocking(AccountCommand::SendMessage {
                    message: Box::new(notification),
                    not_before: None,
                    client_token: None,
                });
            }
            toast_overlay.add_toast(adw::Toast::new("Meeting forwarded"));
        });
    }
    dialog.present(Some(parent));
}

/// Builds and wires the invite card's response buttons (CAL-15 / UX-60):
/// Accept, Tentative and Decline split buttons whose arrows each offer
/// "Send response now", "Edit response before sending" and "Don't send a
/// response". One action group carries the nine menu items, with the answer
/// as the action's string parameter; `render_invite_card` shows the row only
/// for an un-dismissed REQUEST.
pub(crate) fn build_imip_response_actions(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
) {
    let Some(card) = find_named_child(reading_stack, "imip-invite-card").and_then(|child| child.downcast::<gtk::Box>().ok()) else {
        return;
    };
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::Start)
        .margin_top(2)
        .build();
    row.set_widget_name("imip-respond-actions");
    row.set_visible(false);

    let group = gio::SimpleActionGroup::new();
    let send_now = gio::SimpleAction::new("send-now", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let reading_stack = reading_stack.clone();
        send_now.connect_activate(move |_, parameter| {
            let Some(status) = parameter.and_then(|value| value.str()).and_then(response_status_from_param) else {
                return;
            };
            let Some(context) = imip_reply_context(&state) else { return };
            respond_to_imip_invitation(
                &calendar_state,
                &toast_overlay,
                &context.invitation,
                &context.from_email,
                Some(context.display_name.as_str()),
                &context.cmd_tx,
                status,
                true,
                None,
            );
            dismiss_imip(&state, &reading_stack, &context.mailbox, context.uid);
        });
    }
    group.add_action(&send_now);
    let edit_response = gio::SimpleAction::new("edit-response", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        edit_response.connect_activate(move |_, parameter| {
            let Some(status) = parameter.and_then(|value| value.str()).and_then(response_status_from_param) else {
                return;
            };
            let Some(context) = imip_reply_context(&state) else { return };
            open_imip_reply_composer(&state, &calendar_state, &toast_overlay, &worker, &reading_stack, &context, status);
        });
    }
    group.add_action(&edit_response);
    let dont_send = gio::SimpleAction::new("dont-send", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let reading_stack = reading_stack.clone();
        dont_send.connect_activate(move |_, parameter| {
            let Some(status) = parameter.and_then(|value| value.str()).and_then(response_status_from_param) else {
                return;
            };
            let Some(context) = imip_reply_context(&state) else { return };
            respond_to_imip_invitation(
                &calendar_state,
                &toast_overlay,
                &context.invitation,
                &context.from_email,
                Some(context.display_name.as_str()),
                &context.cmd_tx,
                status,
                false,
                None,
            );
            dismiss_imip(&state, &reading_stack, &context.mailbox, context.uid);
        });
    }
    group.add_action(&dont_send);
    // CAL-17: the attendee's "Propose new time…" (hidden by the card when
    // `X-MICROSOFT-DISALLOW-COUNTER` or no organizer is named).
    let propose_time = gio::SimpleAction::new("propose-time", None);
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let toast_overlay = toast_overlay.clone();
        propose_time.connect_activate(move |_, _| {
            let Some(context) = imip_reply_context(&state) else { return };
            show_propose_new_time_dialog(reading_stack.upcast_ref(), &toast_overlay, &context);
        });
    }
    group.add_action(&propose_time);
    // CAL-18: forward the meeting to new recipients (a fresh REQUEST plus a
    // notification to the organizer).
    let forward_meeting = gio::SimpleAction::new("forward-meeting", None);
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let toast_overlay = toast_overlay.clone();
        forward_meeting.connect_activate(move |_, _| {
            let Some(context) = imip_reply_context(&state) else { return };
            let Some(event) = lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), &context.invitation.ics).into_iter().next() else {
                toast_overlay.add_toast(adw::Toast::new("Couldn't parse this invitation's calendar data."));
                return;
            };
            show_forward_meeting_dialog(reading_stack.upcast_ref(), &toast_overlay, &imip_identity(&context), &context.cmd_tx, &event);
        });
    }
    group.add_action(&forward_meeting);
    row.insert_action_group("respond", Some(&group));

    for (status, label, css) in [
        (AttendeeStatus::Accepted, "Accept", Some("suggested-action")),
        (AttendeeStatus::Tentative, "Tentative", None),
        (AttendeeStatus::Declined, "Decline", Some("destructive-action")),
    ] {
        let button = adw::SplitButton::builder().label(label).dropdown_tooltip(format!("{label} options")).build();
        button.set_tooltip_text(Some(label));
        if let Some(css) = css {
            button.add_css_class(css);
        }
        let menu = gio::Menu::new();
        for (item_label, action_name) in [
            ("Send response now", "send-now"),
            ("Edit response before sending", "edit-response"),
            ("Don't send a response", "dont-send"),
        ] {
            let item = gio::MenuItem::new(Some(item_label), None);
            item.set_action_and_target_value(Some(&format!("respond.{action_name}")), Some(&response_status_param(status).to_variant()));
            menu.append_item(&item);
        }
        button.set_menu_model(Some(&menu));
        {
            let group = group.clone();
            button.connect_clicked(move |_| {
                // The main part is Outlook's "Send response now".
                group.activate_action("send-now", Some(&response_status_param(status).to_variant()));
            });
        }
        row.append(&button);
    }
    // The Propose new time / Forward extras sit after the three responses,
    // the way Outlook's Respond group trails its Accept/Tentative/Decline.
    let propose_button = gtk::Button::builder().label("Propose new time").css_classes(["flat"]).build();
    propose_button.set_widget_name("imip-propose-button");
    propose_button.set_tooltip_text(Some("Propose new time"));
    {
        let group = group.clone();
        propose_button.connect_clicked(move |_| {
            group.activate_action("propose-time", None);
        });
    }
    let forward_button = gtk::Button::builder().label("Forward").css_classes(["flat"]).build();
    forward_button.set_widget_name("imip-forward-button");
    forward_button.set_tooltip_text(Some("Forward meeting"));
    {
        let group = group.clone();
        forward_button.connect_clicked(move |_| {
            group.activate_action("forward-meeting", None);
        });
    }
    row.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    row.append(&propose_button);
    row.append(&forward_button);
    card.append(&row);

    // CAL-17: the organizer's decision on a proposed new time. A row of its
    // own (the respond row is for invitations), shown only for a COUNTER.
    let counter_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::Start)
        .margin_top(2)
        .build();
    counter_row.set_widget_name("imip-counter-actions");
    counter_row.set_visible(false);
    let counter_group = gio::SimpleActionGroup::new();
    let accept_proposal = gio::SimpleAction::new("accept-proposal", None);
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let reading_stack = reading_stack.clone();
        accept_proposal.connect_activate(move |_, _| {
            let Some(context) = imip_reply_context(&state) else { return };
            if accept_counter_proposal(&state, &calendar_state, &toast_overlay, &context.invitation) {
                dismiss_imip(&state, &reading_stack, &context.mailbox, context.uid);
            }
        });
    }
    counter_group.add_action(&accept_proposal);
    let decline_proposal = gio::SimpleAction::new("decline-proposal", None);
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let reading_stack = reading_stack.clone();
        decline_proposal.connect_activate(move |_, _| {
            let Some(context) = imip_reply_context(&state) else { return };
            if decline_counter_proposal(&state, &calendar_state, &toast_overlay, &context.invitation) {
                dismiss_imip(&state, &reading_stack, &context.mailbox, context.uid);
            }
        });
    }
    counter_group.add_action(&decline_proposal);
    counter_row.insert_action_group("counter", Some(&counter_group));
    let accept_button = gtk::Button::with_label("Accept proposal");
    accept_button.add_css_class("suggested-action");
    {
        let counter_group = counter_group.clone();
        accept_button.connect_clicked(move |_| {
            counter_group.activate_action("accept-proposal", None);
        });
    }
    let decline_button = gtk::Button::with_label("Decline proposal");
    decline_button.add_css_class("destructive-action");
    {
        let counter_group = counter_group.clone();
        decline_button.connect_clicked(move |_| {
            counter_group.activate_action("decline-proposal", None);
        });
    }
    counter_row.append(&accept_button);
    counter_row.append(&decline_button);
    card.append(&counter_row);
}

/// The organizer cancelled an event (iMIP `METHOD:CANCEL`): removes the
/// stored calendar object when one exists under the same UID, otherwise just
/// acknowledges. The banner was already dismissed by the caller.
pub(crate) fn remove_cancelled_imip_event(calendar_state: &Rc<RefCell<CalendarUiState>>, toast_overlay: &adw::ToastOverlay, invitation: &lookout_core::ImipInvitation) {
    let Some(mut event) = lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), &invitation.ics).into_iter().next() else {
        toast_overlay.add_toast(adw::Toast::new("Couldn't parse this cancellation's calendar data."));
        return;
    };
    match find_calendar_occurrence(calendar_state, &event.uid) {
        Some(existing) => {
            event.calendar_id = existing.calendar_id.clone();
            event.href = existing.href;
            event.etag = existing.etag;
            if let Some(handle) = calendar_handle_for_id(calendar_state, &event.calendar_id) {
                if let Some(href) = event.href.clone() {
                    let _ = handle.send_blocking(CalendarCommand::DeleteEvent {
                        calendar_id: event.calendar_id.clone(),
                        href,
                        etag: event.etag.clone(),
                    });
                    toast_overlay.add_toast(adw::Toast::new("Event removed from calendar"));
                    return;
                }
            }
            toast_overlay.add_toast(adw::Toast::new("Couldn't remove the event from your calendar."));
        }
        None => {
            toast_overlay.add_toast(adw::Toast::new("This event wasn't in your calendar."));
        }
    }
}

/// The stored occurrence matching `uid` inside `calendar_id` specifically.
/// (The iMIP upsert searches every calendar; an `.ics` import is scoped to
/// its target instead, since re-importing a file must update what the file
/// previously added, not touch a coincidental same-UID event elsewhere.)
pub(crate) fn find_occurrence_in_calendar(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId, uid: &EventUid) -> Option<EventOccurrence> {
    calendar_state
        .borrow()
        .accounts
        .values()
        .flat_map(|handle| handle.last_occurrences.iter())
        .find(|occurrence| occurrence.uid == *uid && occurrence.calendar_id == *calendar_id)
        .cloned()
}

/// The "Add calendar" dialog - the sidebar button's one-stop entry point for
/// the whole subscription/import story:
///
/// * **Subscribe** - a webcal/`https://…ics` feed URL (+ optional name) is
///   validated, persisted to `settings.json`, and handed to the webcal
///   session as a full `Reload` list (the session never learns of partial
///   changes - the list here is always authoritative).
/// * **Import** - a picked `.ics` file's events are routed into a chosen
///   CalDAV calendar through the existing session write path, upserting by
///   UID so re-importing the same file updates rather than duplicates.
/// * **Manage** - the current subscriptions, each with a Remove action, plus
///   a manual "Refresh now" for feeds that shouldn't wait for the poll.
///
/// Subscriptions are fetch-only, so import targets only ever list CalDAV
/// calendars (`pickable_calendars`), and removing a subscription also drops
/// its feed cache, its checked flag, and its colour assignment.
#[allow(clippy::too_many_arguments)]
/// The manage-list rebuild slot used by [`show_calendar_dialog`]: a closure
/// `Option` behind a `RefCell`, since the rebuild closure and the remove
/// handler are mutually referential (the slot breaks the cycle).
pub(crate) type ManageRebuild = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// One row of the reading pane's invite-details card: a dim caption above a
/// wrapping value label. Both the row and its value label get `widget_name`s
/// (`row_name`/`value_name`) so `render_invite_card` can find them and hide a
/// row wholesale when the invitation lacks that property.
pub(crate) fn invite_detail_row(row_name: &str, value_name: &str, caption: &str) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(1).build();
    row.set_widget_name(row_name);
    let caption_label = gtk::Label::new(Some(caption));
    caption_label.add_css_class("dim-label");
    caption_label.add_css_class("caption");
    crate::privacy::mark_ui(&caption_label);
    caption_label.set_xalign(0.0);
    let value = gtk::Label::new(None);
    value.set_widget_name(value_name);
    value.set_xalign(0.0);
    value.set_wrap(true);
    value.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    // The Organizer line is a name/address: blurred by privacy mode.
    if value_name == "imip-organizer" {
        crate::privacy::mark(&value);
    }
    row.append(&caption_label);
    row.append(&value);
    row
}

/// Sets one invite-card row's value label, hiding the row when the invitation
/// has nothing for it. `text == None` hides the row; `Some("")` is not
/// expected but would show an empty value (callers only pass `Some` for
/// payloads that actually carry the property).
pub(crate) fn set_invite_row(card: &gtk::Box, row_name: &str, value_name: &str, text: Option<String>) {
    let Some(row) = named_child_of(card, row_name).and_then(|r| r.downcast::<gtk::Box>().ok()) else {
        return;
    };
    row.set_visible(text.is_some());
    if let Some(label) = named_child_of(&row, value_name).and_then(|l| l.downcast::<gtk::Label>().ok()) {
        label.set_label(text.as_deref().unwrap_or(""));
    }
}

/// Renders the invite card's "When" line: the event's start/end in local time
/// (all-day entries as dates, with `DTEND`'s exclusivity undone for the last
/// shown day), plus a recurring hint when the master carries an `RRULE`.
/// Follows the 12-hour style the calendar views and the message header use.
pub(crate) fn format_imip_when(invitation: &lookout_core::ImipInvitation) -> String {
    let recurring = if invitation.rrule.is_some() { " · Recurring" } else { "" };
    let (start, end) = crate::calendar_tz::occurrence_span(crate::calendar_tz::display_zone(), invitation.start, invitation.end, invitation.all_day);
    if invitation.all_day {
        if end.date() <= start.date() + chrono::Duration::days(1) {
            return format!("All day · {}{}", start.format("%a %d %b %Y"), recurring);
        }
        // DTEND is exclusive: the last day shown is the one before it.
        let last_day = end.date() - chrono::Duration::days(1);
        return format!("All day · {} – {}{}", start.format("%a %d %b %Y"), last_day.format("%a %d %b %Y"), recurring);
    }
    if end.date() == start.date() {
        format!("{} · {} – {}{}", start.format("%a %d %b %Y"), start.format("%I:%M %p"), end.format("%I:%M %p"), recurring)
    } else {
        format!(
            "{} {} – {} {}{}",
            start.format("%a %d %b %Y"),
            start.format("%I:%M %p"),
            end.format("%a %d %b %Y"),
            end.format("%I:%M %p"),
            recurring
        )
    }
}

/// The invite card's "Organizer" line: the display name (when the payload
/// carries one) alongside the address, so the reply's recipient is unambiguous.
pub(crate) fn format_imip_organizer(organizer: &lookout_core::EmailAddress) -> String {
    match &organizer.name {
        Some(name) if !name.trim().is_empty() => format!("{} <{}>", name, organizer.address),
        _ => organizer.address.clone(),
    }
}

/// The invite card's "Conflicts with …" value: the conflicting events' names,
/// or `None` when nothing overlaps (which hides the row).
pub(crate) fn conflict_summary(conflicts: &[EventOccurrence]) -> Option<String> {
    if conflicts.is_empty() {
        return None;
    }
    let names: Vec<String> = conflicts
        .iter()
        .map(|occurrence| {
            occurrence.summary.clone().filter(|summary| !summary.trim().is_empty()).unwrap_or_else(|| {
                if occurrence.all_day {
                    "an all-day event".to_string()
                } else {
                    "an event".to_string()
                }
            })
        })
        .collect();
    Some(names.join(", "))
}

/// The invite card's CAL-35 data: the user's cached occurrences that clash
/// with the invitation, plus a mini day timeline of the invite's first day.
/// Empty/`None` when there's no calendar state, or the invite's month isn't in
/// the in-memory window (the card then just omits both).
pub(crate) fn invite_card_extras(
    calendar_state: &Option<Rc<RefCell<CalendarUiState>>>,
    prefs: &crate::settings::CalendarPreferences,
    invitation: &lookout_core::ImipInvitation,
) -> (Vec<EventOccurrence>, Option<gtk::Widget>) {
    let Some(calendar_state) = calendar_state else {
        return (Vec::new(), None);
    };
    let zone = crate::calendar_tz::display_zone();
    let (start_local, _) = crate::calendar_tz::occurrence_span(zone, invitation.start, invitation.end, invitation.all_day);
    let day = start_local.date();
    let (conflicts, intervals) = {
        let st = calendar_state.borrow();
        let occurrences = st.checked_occurrences_for_month(day);
        let conflicts: Vec<EventOccurrence> = crate::invite_conflicts::conflicting_occurrences(&occurrences, invitation.start, invitation.end, &invitation.uid)
            .into_iter()
            .cloned()
            .collect();
        let intervals = crate::invite_conflicts::busy_intervals(&occurrences, &invitation.uid);
        (conflicts, intervals)
    };
    // An all-day invite has no meaningful time span to outline; a timed one
    // gets its proposed slot.
    let proposed = (!invitation.all_day).then(|| crate::calendar_tz::occurrence_span(zone, invitation.start, invitation.end, false));
    let timeline = if intervals.is_empty() && proposed.is_none() {
        None
    } else {
        Some(crate::schedule_grid::build_day_timeline(prefs, day, zone, intervals, proposed))
    };
    (conflicts, timeline)
}

/// Repopulates the reading pane's invite-details card for the invitation
/// being rendered, hiding it when the message carries none (or the user
/// dismissed it). Complements the `adw::Banner` at the bottom of the page,
/// which only fits a title and a button.
pub(crate) fn render_invite_card(reading_stack: &gtk::Stack, invitation: Option<&lookout_core::ImipInvitation>, conflicts: &[EventOccurrence], timeline: Option<gtk::Widget>) {
    let Some(card) = find_named_child(reading_stack, "imip-invite-card").and_then(|c| c.downcast::<gtk::Box>().ok()) else {
        return;
    };
    card.set_visible(invitation.is_some());
    let Some(invitation) = invitation else { return };
    set_invite_row(&card, "imip-when-row", "imip-when", Some(format_imip_when(invitation)));
    set_invite_row(&card, "imip-where-row", "imip-where", invitation.location.clone());
    set_invite_row(&card, "imip-organizer-row", "imip-organizer", invitation.organizer.as_ref().map(format_imip_organizer));
    set_invite_row(&card, "imip-description-row", "imip-description", invitation.description.clone());
    // CAL-35 / 3.18: the conflict line ("Conflicts with Standup, 1:1") and the
    // mini day timeline, both from `invite_card_extras`.
    set_invite_row(&card, "imip-conflict-row", "imip-conflict", conflict_summary(conflicts));
    if let Some(container) = find_named_descendant(&card, "imip-timeline").and_then(|w| w.downcast::<gtk::Box>().ok()) {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        match timeline {
            Some(widget) => {
                container.append(&widget);
                container.set_visible(true);
            }
            None => container.set_visible(false),
        }
    }
    // CAL-20 / 3.17: the online-meeting Join button, shown only when the
    // invitation carries a join URL, labelled by provider ("Join Teams").
    if let Some(join) = find_named_descendant(&card, "imip-join-button").and_then(|w| w.downcast::<gtk::LinkButton>().ok()) {
        match invitation.conference_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(url) => {
                let label = match lookout_core::interop::onlinemeeting::classify(url) {
                    Some(provider) => format!("Join {}", provider.display_name()),
                    None => "Join".to_string(),
                };
                join.set_label(&label);
                join.set_uri(url);
                join.set_visible(true);
            }
            None => join.set_visible(false),
        }
    }
    // The response buttons (CAL-15) are for an invitation the user can
    // answer - a REQUEST. CANCEL and REPLY keep the details-only card, while
    // a COUNTER (a proposed new time, CAL-17) swaps the respond row for the
    // organizer's accept/decline pair. The Propose/Forward extras follow the
    // invitation's own constraints: no organizer means nothing to send to,
    // and `X-MICROSOFT-DISALLOW-COUNTER` hides the proposal.
    if let Some(actions) = named_child_of(&card, "imip-respond-actions") {
        actions.set_visible(matches!(invitation.method, lookout_core::ImipMethod::Request));
        if let Some(propose) = find_named_descendant(&actions, "imip-propose-button") {
            propose.set_visible(!invitation.disallow_counter && invitation.organizer.is_some());
        }
        if let Some(forward) = find_named_descendant(&actions, "imip-forward-button") {
            forward.set_visible(invitation.organizer.is_some());
        }
    }
    if let Some(counter_actions) = named_child_of(&card, "imip-counter-actions") {
        counter_actions.set_visible(matches!(invitation.method, lookout_core::ImipMethod::Counter));
    }
}
