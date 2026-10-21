//! The profile card (Phase 4.2 / P-10, UX-67): a popover showing a person's
//! contact details, their recent mail, upcoming shared events and a small set
//! of actions, opened by hovering or clicking any person reference - the
//! reading-pane sender, a message-list row's sender/avatar, a conversation
//! card's sender, an event editor's attendee chip, an invitation card's
//! organizer, or a People row.
//!
//! One reusable component (`attach_person_trigger`) serves every surface so
//! placement, wording and behaviour stay identical everywhere (§6.1 rule 4).
//! The card itself is data-in/widget-out: the host window supplies the
//! [`ProfileCardActions`] closures that open composers, events and contact
//! dialogs, so this module never reaches into session or window internals.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{Duration, NaiveDateTime};
use gtk::glib;
use lookout_core::{AccountId, EmailAddress, EmailSummary, EventOccurrence, VCard};

use crate::util::action_button;
use crate::window::UiState;
use crate::worker::Worker;

/// How long the pointer must rest on a person reference before the hover card
/// opens (a shorter delay than a tooltip, matching Outlook's quick reveal).
const HOVER_DELAY_MS: u64 = 350;
/// How many recent messages the card lists.
const RECENT_MAIL_LIMIT: usize = 5;
/// How many upcoming shared events the card lists.
const UPCOMING_EVENTS_LIMIT: usize = 3;
/// How far ahead the events section looks.
const EVENT_HORIZON_DAYS: i64 = 90;

/// A lazily-resolved person reference: the address plus the account it came
/// from (which the composer should send from, when known).
pub type PersonResolver = Rc<dyn Fn() -> Option<(EmailAddress, Option<AccountId>)>>;
/// A late-bound provider of the window's card context: the message-list rows
/// are built before the context exists, so they resolve it at click time.
pub type ContextProvider = Rc<dyn Fn() -> Option<Rc<ProfileCardContext>>>;
/// A card action that acts on one person.
pub type PersonActionFn = Rc<dyn Fn(&EmailAddress)>;
/// "Send email": the person plus the account to send from.
pub type SendEmailFn = Rc<dyn Fn(&EmailAddress, Option<AccountId>)>;
/// "Open message": the message to open.
pub type OpenMessageFn = Rc<dyn Fn(&EmailSummary)>;
/// "Open event": the occurrence to open.
pub type OpenEventFn = Rc<dyn Fn(&EventOccurrence)>;
/// The occurrences a person organizes or attends.
pub type EventsForPersonFn = Rc<dyn Fn(&str) -> Vec<EventOccurrence>>;

/// The actions a card offers, built by the host window (which owns the
/// reading pane, sessions and dialogs). Every closure takes the person the
/// card is about, so one [`ProfileCardActions`] serves every trigger.
#[derive(Clone)]
pub struct ProfileCardActions {
    /// Open a blank composer addressed to the person, from the given account
    /// (or the default account when `None`).
    pub send_email: SendEmailFn,
    /// Open a new event with the person as an attendee.
    pub schedule_meeting: PersonActionFn,
    /// Open the blank "new contact" editor prefilled with the person.
    pub add_contact: PersonActionFn,
    /// Open the existing contact in the editor.
    pub edit_contact: PersonActionFn,
    /// Open a recent message in the reading pane.
    pub open_message: OpenMessageFn,
    /// Open an upcoming shared event in the event editor.
    pub open_event: OpenEventFn,
    /// Every occurrence (from any calendar) that the given address organizes
    /// or attends. The card filters these to the upcoming ones itself.
    pub events_for_person: EventsForPersonFn,
}

/// Everything the card needs to render and act, assembled once per window.
#[derive(Clone)]
pub struct ProfileCardContext {
    pub state: Rc<RefCell<UiState>>,
    pub worker: Rc<Worker>,
    pub actions: ProfileCardActions,
}

/// Whether an occurrence involves `address` as organizer or attendee.
fn occurrence_involves(occ: &EventOccurrence, address: &str) -> bool {
    occ.organizer.as_ref().is_some_and(|a| a.address.eq_ignore_ascii_case(address)) || occ.attendees.iter().any(|a| a.address.address.eq_ignore_ascii_case(address))
}

/// The upcoming occurrences `address` is part of, sorted by start and capped
/// at `limit`. Pure so it can be unit-tested without a GTK loop; `now` and
/// the occurrence starts are compared as local wall-clock times in the
/// calendar's display zone, where an all-day event starts at 00:00.
pub fn shared_upcoming_events(occurrences: &[EventOccurrence], address: &str, now: NaiveDateTime, horizon: Duration, limit: usize) -> Vec<EventOccurrence> {
    let zone = crate::calendar_tz::display_zone();
    let horizon_end = now + horizon;
    let mut upcoming: Vec<EventOccurrence> = occurrences
        .iter()
        .filter(|occ| occurrence_involves(occ, address))
        .filter(|occ| {
            let start = zone.occurrence_to_local(occ.start, occ.all_day);
            start >= now && start <= horizon_end
        })
        .cloned()
        .collect();
    upcoming.sort_by_key(|occ| occ.start);
    upcoming.truncate(limit);
    upcoming
}

/// Attaches the profile-card trigger to `widget`: a click opens the card
/// immediately, and hovering for [`HOVER_DELAY_MS`] opens it too. `resolve`
/// is consulted lazily at event time (so recycled list rows and WebView
/// callbacks always describe the right person), returning the address and the
/// account the reference belongs to; `context` is consulted lazily too, so a
/// trigger can be installed before the window's [`ProfileCardContext`] is
/// ready (the message-list rows are built before the calendar state exists).
pub fn attach_person_trigger<W: IsA<gtk::Widget>>(context: ContextProvider, widget: &W, claim_press: bool, resolve: PersonResolver) {
    let widget = widget.clone().upcast::<gtk::Widget>();
    let active: Rc<RefCell<Option<gtk::Popover>>> = Rc::new(RefCell::new(None));
    let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    // Click opens straight away. On the reading-pane header the press is
    // claimed (there's nothing beneath to select); on list rows it is left
    // unclaimed so the row still selects and drags as usual.
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    {
        let context = context.clone();
        let widget = widget.clone();
        let resolve = resolve.clone();
        let active = active.clone();
        let timer = timer.clone();
        click.connect_pressed(move |gesture, _n, _x, _y| {
            if claim_press {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
            cancel_timer(&timer);
            show_card(&context, &widget, &resolve, &active);
        });
    }
    widget.add_controller(click);

    // Hover opens after a short rest; leaving cancels the pending open.
    let motion = gtk::EventControllerMotion::new();
    {
        let context = context.clone();
        let widget = widget.clone();
        let resolve = resolve.clone();
        let active = active.clone();
        let timer = timer.clone();
        motion.connect_enter(move |_, _x, _y| {
            if active.borrow().is_some() {
                return;
            }
            let context = context.clone();
            let widget = widget.clone();
            let resolve = resolve.clone();
            let active = active.clone();
            arm_timer(&timer, std::time::Duration::from_millis(HOVER_DELAY_MS), move || {
                show_card(&context, &widget, &resolve, &active);
            });
        });
    }
    {
        let timer = timer.clone();
        motion.connect_leave(move |_| cancel_timer(&timer));
    }
    widget.add_controller(motion);
}

/// Arms the hover-open timer in `timer`, replacing any pending one. The
/// callback clears the slot before running `f`: once a one-shot source has
/// fired GLib has destroyed it, and a later `cancel_timer` removing that stale
/// id would panic in the (non-unwindable) GTK signal trampoline.
fn arm_timer(timer: &Rc<RefCell<Option<glib::SourceId>>>, delay: std::time::Duration, f: impl FnOnce() + 'static) {
    cancel_timer(timer);
    let slot = timer.clone();
    let src = glib::timeout_add_local_once(delay, move || {
        slot.borrow_mut().take();
        f();
    });
    *timer.borrow_mut() = Some(src);
}

/// Cancels a pending hover-open timer, if any.
fn cancel_timer(timer: &Rc<RefCell<Option<glib::SourceId>>>) {
    if let Some(src) = timer.borrow_mut().take() {
        src.remove();
    }
}

/// Closes any open card and shows a fresh one anchored to `widget`.
fn show_card(context: &ContextProvider, widget: &gtk::Widget, resolve: &PersonResolver, active: &Rc<RefCell<Option<gtk::Popover>>>) {
    // Drop the previous card first (popdown emits `closed`, which clears the
    // slot itself, so take before pulling it down to avoid a double borrow).
    let previous = active.borrow_mut().take();
    if let Some(previous) = previous {
        previous.popdown();
        previous.unparent();
    }
    let Some(ctx) = context() else { return };
    let Some((address, account_id)) = resolve() else { return };

    let popover = gtk::Popover::builder().autohide(true).has_arrow(true).build();
    let content = build_card(&ctx, &address, account_id.as_ref());
    let scroller = gtk::ScrolledWindow::builder()
        .child(&content)
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .max_content_height(560)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    popover.set_child(Some(&scroller));
    popover.set_parent(widget);
    {
        let active = active.clone();
        popover.connect_closed(move |p| {
            p.unparent();
            let mut slot = active.borrow_mut();
            if slot.as_ref().is_some_and(|current| std::ptr::eq(current, p)) {
                *slot = None;
            }
        });
    }
    popover.popup();
    *active.borrow_mut() = Some(popover);
}

/// Builds the card's contents for one person.
fn build_card(ctx: &ProfileCardContext, address: &EmailAddress, account_id: Option<&AccountId>) -> gtk::Widget {
    let contact = crate::contacts_view::find_contact_by_address(&ctx.state, &address.address, account_id).map(|(_, entry)| Rc::unwrap_or_clone(entry.record).card);

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .width_request(320)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(10)
        .margin_end(10)
        .build();
    root.add_css_class("profile-card");

    root.append(&build_header(address, contact.as_ref()));
    root.append(&build_actions(ctx, address, contact.is_some(), account_id.cloned()));
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // --- Recent mail (loaded asynchronously from the cache). ---
    root.append(&section_heading("Recent mail"));
    let recent_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    root.append(&recent_box);
    spawn_recent_mail(ctx, address, account_id, recent_box.clone());

    // --- Upcoming shared events (already in memory, so synchronous). ---
    let events = {
        let now = chrono::Local::now().naive_local();
        let horizon = Duration::days(EVENT_HORIZON_DAYS);
        shared_upcoming_events(&(ctx.actions.events_for_person)(&address.address), &address.address, now, horizon, UPCOMING_EVENTS_LIMIT)
    };
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&section_heading("Upcoming events"));
    if events.is_empty() {
        root.append(&empty_label("No upcoming shared events"));
    } else {
        for occ in &events {
            root.append(&event_row(ctx, occ));
        }
    }

    root.upcast()
}

/// The card's header: avatar/photo, display name, title · organization and
/// the address itself.
fn build_header(address: &EmailAddress, contact: Option<&VCard>) -> gtk::Widget {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(10).build();

    let (avatar, initials_label, picture) = avatar_widget(48);
    initials_label.set_label(&crate::message_header::initials(address.name.as_deref(), &address.address));
    initials_label.add_css_class(crate::message_header::avatar_color_class(&address.address));
    if let Some(texture) = contact.and_then(|card| card.photo.as_ref()).and_then(crate::contacts_view::contact_photo_texture) {
        picture.set_paintable(Some(&texture));
    }
    row.append(&avatar);
    // The avatar (initials/photo) identifies the person: blurred by privacy mode.
    crate::privacy::mark(&avatar);

    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(1)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let name = contact.map(crate::contacts_view::vcard_display_name).unwrap_or_else(|| address.display_label().to_string());
    let name_label = gtk::Label::builder().label(&name).xalign(0.0).wrap(true).css_classes(["heading"]).build();
    text.append(&name_label);

    let secondary = contact.and_then(|card| {
        let org = card
            .organization
            .as_ref()
            .map(|parts| parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" / "))
            .filter(|s| !s.is_empty());
        let title = card.title.as_deref().map(str::trim).filter(|s| !s.is_empty());
        match (title, org) {
            (Some(t), Some(o)) => Some(format!("{t} · {o}")),
            (Some(t), None) => Some(t.to_string()),
            (None, Some(o)) => Some(o),
            (None, None) => None,
        }
    });
    if let Some(secondary) = secondary {
        text.append(&gtk::Label::builder().label(&secondary).xalign(0.0).wrap(true).css_classes(["caption", "dim-label"]).build());
    }
    if let Some(phones) = contact.map(|card| {
        card.telephones
            .iter()
            .map(|phone| phone.number.trim())
            .filter(|number| !number.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
    }) {
        if !phones.is_empty() {
            text.append(&gtk::Label::builder().label(&phones).xalign(0.0).wrap(true).css_classes(["caption", "dim-label"]).build());
        }
    }
    text.append(&gtk::Label::builder().label(&address.address).xalign(0.0).wrap(true).css_classes(["caption"]).build());
    // Name, title/organization, phones and address all live in this column.
    crate::privacy::mark(&text);
    row.append(&text);

    row.upcast()
}

/// The action row: Send email · Schedule meeting · Add/Edit contact · People.
fn build_actions(ctx: &ProfileCardContext, address: &EmailAddress, has_contact: bool, account_id: Option<AccountId>) -> gtk::Widget {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).build();

    let send = action_button("mail-send-symbolic", "Send email");
    {
        let actions = ctx.actions.clone();
        let address = address.clone();
        let account_id = account_id.clone();
        send.connect_clicked(move |_| (actions.send_email)(&address, account_id.clone()));
    }
    row.append(&send);

    let schedule = action_button("x-office-calendar-symbolic", "Schedule");
    {
        let actions = ctx.actions.clone();
        let address = address.clone();
        schedule.connect_clicked(move |_| (actions.schedule_meeting)(&address));
    }
    row.append(&schedule);

    if has_contact {
        let edit = action_button("document-edit-symbolic", "Edit contact");
        {
            let actions = ctx.actions.clone();
            let address = address.clone();
            edit.connect_clicked(move |_| (actions.edit_contact)(&address));
        }
        row.append(&edit);
    } else {
        let add = action_button("contact-new-symbolic", "Add to contacts");
        {
            let actions = ctx.actions.clone();
            let address = address.clone();
            add.connect_clicked(move |_| (actions.add_contact)(&address));
        }
        row.append(&add);
    }

    row.upcast()
}

/// Loads the person's most recent cached messages off the GTK thread and
/// fills `container` (or a dim "No recent messages" note) when they arrive.
fn spawn_recent_mail(ctx: &ProfileCardContext, address: &EmailAddress, account_id: Option<&AccountId>, container: gtk::Box) {
    let query = lookout_mail::SearchQuery {
        from: vec![address.address.clone()],
        ..Default::default()
    };
    let caches: Vec<std::sync::Arc<lookout_mail::Cache>> = {
        let state = ctx.state.borrow();
        match account_id {
            Some(id) => state.accounts.get(id).and_then(|handle| handle.address_cache.clone()).into_iter().collect(),
            None => state.accounts.values().filter_map(|handle| handle.address_cache.clone()).collect(),
        }
    };
    if caches.is_empty() {
        container.append(&empty_label("No recent messages"));
        return;
    }

    let receivers: Vec<async_channel::Receiver<Vec<EmailSummary>>> = caches
        .into_iter()
        .map(|cache| {
            let query = query.clone();
            crate::window::spawn_cache_read(&ctx.worker, cache, move |cache| cache.search_query(&query, None, RECENT_MAIL_LIMIT).unwrap_or_default())
        })
        .collect();

    let actions = ctx.actions.clone();
    glib::spawn_future_local(async move {
        let mut messages: Vec<EmailSummary> = Vec::new();
        for receiver in receivers {
            if let Ok(found) = receiver.recv().await {
                messages.extend(found);
            }
        }
        messages.sort_by_key(|message| std::cmp::Reverse(message.date));
        messages.truncate(RECENT_MAIL_LIMIT);
        if messages.is_empty() {
            container.append(&empty_label("No recent messages"));
            return;
        }
        for message in &messages {
            container.append(&message_row(&actions, message));
        }
    });
}

/// One recent-mail row: the subject, with the date on the right.
fn message_row(actions: &ProfileCardActions, message: &EmailSummary) -> gtk::Widget {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    let subject = gtk::Label::builder()
        .label(message.subject.as_deref().unwrap_or("(no subject)"))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let date = gtk::Label::builder()
        .label(message.date.with_timezone(&chrono::Local).format("%d/%m/%Y").to_string())
        .css_classes(["caption", "dim-label"])
        .valign(gtk::Align::Center)
        .build();
    row.append(&subject);
    row.append(&date);

    let button = gtk::Button::builder().child(&row).css_classes(["flat", "profile-card-row"]).build();
    // Subject/date are content: blurred by privacy mode.
    crate::privacy::mark_content(&button);
    let actions = actions.clone();
    let message = message.clone();
    button.connect_clicked(move |_| (actions.open_message)(&message));
    // Close the popover after the message opens.
    button.upcast()
}

/// One upcoming-event row: the title, with its date/time underneath.
fn event_row(ctx: &ProfileCardContext, occ: &EventOccurrence) -> gtk::Widget {
    let (start, _end) = crate::calendar_view::local_span(occ);
    let time = if occ.all_day {
        start.format("%a %d %b").to_string()
    } else {
        start.format("%a %d %b %H:%M").to_string()
    };

    let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(0).hexpand(true).build();
    text.append(
        &gtk::Label::builder()
            .label(occ.summary.as_deref().unwrap_or("(no subject)"))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    text.append(&gtk::Label::builder().label(&time).xalign(0.0).css_classes(["caption", "dim-label"]).build());

    let button = gtk::Button::builder().child(&text).css_classes(["flat", "profile-card-row"]).build();
    // Event title/time are content: blurred by privacy mode.
    crate::privacy::mark_content(&button);
    let actions = ctx.actions.clone();
    let occ = occ.clone();
    button.connect_clicked(move |_| (actions.open_event)(&occ));
    button.upcast()
}

/// A section caption ("Recent mail", "Upcoming events").
fn section_heading(text: &str) -> gtk::Widget {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .margin_top(2)
        .build()
        .upcast()
}

/// A dim empty-state note.
fn empty_label(text: &str) -> gtk::Widget {
    let label = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .margin_top(2)
        .margin_bottom(2)
        .build();
    crate::privacy::mark_ui(&label);
    label.upcast()
}

/// A flat icon + label button, matching the reading pane's action cluster.
/// The shared avatar widget: a coloured circle with initials, and an optional
/// photo over it (a photo with no paintable leaves the initials visible).
fn avatar_widget(size: i32) -> (gtk::Overlay, gtk::Label, gtk::Picture) {
    let label = gtk::Label::builder().css_classes(["avatar-circle"]).build();
    let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).css_classes(["avatar-photo"]).build();
    let overlay = gtk::Overlay::builder()
        .width_request(size)
        .height_request(size)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .child(&label)
        .build();
    overlay.add_overlay(&picture);
    (overlay, label, picture)
}

/// Shows the card anchored at a point inside `anchor` (window-relative
/// `(x, y)`) - the conversation WebView's route, where a person reference
/// posts its click coordinates rather than being a GTK widget of its own.
/// The open card is parked in a thread-local so it survives until the next
/// call (GTK's `set_parent` does not take ownership).
pub fn show_person_card_at(context: ContextProvider, anchor: &impl IsA<gtk::Widget>, address: &EmailAddress, account_id: Option<AccountId>, x: f64, y: f64) {
    thread_local! {
        static DETACHED_CARD: RefCell<Option<gtk::Popover>> = const { RefCell::new(None) };
    }
    DETACHED_CARD.with(|slot| {
        if let Some(previous) = slot.borrow_mut().take() {
            previous.popdown();
            previous.unparent();
        }
    });
    let Some(ctx) = context() else { return };
    let popover = gtk::Popover::builder().autohide(true).has_arrow(true).build();
    let content = build_card(&ctx, address, account_id.as_ref());
    let scroller = gtk::ScrolledWindow::builder()
        .child(&content)
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .max_content_height(560)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    popover.set_child(Some(&scroller));
    popover.set_parent(&anchor.clone().upcast::<gtk::Widget>());
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.popup();
    DETACHED_CARD.with(|slot| *slot.borrow_mut() = Some(popover));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lookout_core::{Attendee, AttendeeRole, CalendarId, EventUid};

    fn occurrence(summary: &str, start: chrono::DateTime<Utc>, organizer: Option<&str>, attendees: &[&str]) -> EventOccurrence {
        EventOccurrence {
            uid: EventUid(format!("{summary}@example.com")),
            calendar_id: CalendarId("cal1".into()),
            summary: Some(summary.to_string()),
            description: None,
            location: None,
            start,
            end: start + Duration::hours(1),
            all_day: false,
            time_zone: None,
            rrule: None,
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
            sequence: 0,
            attendees: attendees
                .iter()
                .map(|a| Attendee {
                    address: EmailAddress::new(*a),
                    role: AttendeeRole::Required,
                    status: Default::default(),
                })
                .collect(),
            organizer: organizer.map(EmailAddress::new),
            schedule_agent: None,
            categories: Vec::new(),
            sensitivity: Default::default(),
            show_as: Default::default(),
            status: Default::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    #[test]
    fn shared_upcoming_events_keeps_only_the_persons_future_events() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap().naive_utc();
        let past = occurrence("Past", Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap(), None, &["ada@example.com"]);
        let attended = occurrence("Standup", Utc.with_ymd_and_hms(2026, 6, 2, 9, 0, 0).unwrap(), None, &["ada@example.com"]);
        let organized = occurrence("Review", Utc.with_ymd_and_hms(2026, 6, 3, 9, 0, 0).unwrap(), Some("ada@example.com"), &[]);
        let unrelated = occurrence("Other", Utc.with_ymd_and_hms(2026, 6, 4, 9, 0, 0).unwrap(), None, &["bob@example.com"]);

        let result = shared_upcoming_events(&[past, attended, organized, unrelated], "ada@example.com", now, Duration::days(90), 10);
        let summaries: Vec<&str> = result.iter().filter_map(|occ| occ.summary.as_deref()).collect();
        assert_eq!(summaries, vec!["Standup", "Review"]);
    }

    #[test]
    fn shared_upcoming_events_matches_organizer_case_insensitively() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap().naive_utc();
        let occ = occurrence("Review", Utc.with_ymd_and_hms(2026, 6, 3, 9, 0, 0).unwrap(), Some("ADA@example.com"), &[]);
        assert_eq!(shared_upcoming_events(&[occ], "ada@example.com", now, Duration::days(90), 10).len(), 1);
    }

    #[test]
    fn shared_upcoming_events_respects_the_limit() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap().naive_utc();
        let occurrences: Vec<EventOccurrence> = (0..5)
            .map(|i| occurrence("E", Utc.with_ymd_and_hms(2026, 6, 2 + i, 9, 0, 0).unwrap(), None, &["ada@example.com"]))
            .collect();
        assert_eq!(shared_upcoming_events(&occurrences, "ada@example.com", now, Duration::days(90), 3).len(), 3);
    }

    #[test]
    fn hover_timer_clears_its_slot_when_it_fires() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let fired = Rc::new(std::cell::Cell::new(false));
        let fired_in_timer = fired.clone();
        arm_timer(&timer, std::time::Duration::ZERO, move || fired_in_timer.set(true));
        let context = glib::MainContext::default();
        while !fired.get() {
            context.iteration(true);
        }
        assert!(timer.borrow().is_none());
        // Leaving or clicking after the card opened: removing the fired
        // source's stale id used to panic here.
        cancel_timer(&timer);
    }

    #[test]
    fn cancelled_hover_timer_never_fires() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let fired = Rc::new(std::cell::Cell::new(false));
        let fired_in_timer = fired.clone();
        arm_timer(&timer, std::time::Duration::ZERO, move || fired_in_timer.set(true));
        cancel_timer(&timer);
        assert!(timer.borrow().is_none());
        let context = glib::MainContext::default();
        while context.iteration(false) {}
        assert!(!fired.get());
    }
}
