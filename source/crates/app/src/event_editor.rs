//! Modal event editor (create/edit/delete) for the Calendar module - the
//! counterpart of `compose.rs`'s mail composer, in dialog form since the
//! calendar has no reading-pane slot to host an inline editor.
//!
//! Data-in/widget-state-out like the rest of the calendar code: the caller
//! supplies the calendars a new event could land in, which one to preselect,
//! (for an edit) the occurrence being changed, and a read-only snapshot of
//! nearby occurrences/colors for the right-hand preview panel, then
//! registers two callbacks - `on_save(CalendarId, CalendarEvent)` for both
//! create and edit (the create vs update distinction falls out of the
//! event's href/etag), and `on_delete(CalendarId, EventUid, href, etag)` -
//! which the dialog never talks to the network to fulfil itself; the caller
//! routes the produced [`CalendarEvent`] to the owning account's session.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Timelike, Utc};
use gtk::{gio, glib};
use lookout_core::{
    Attendee, AttendeeAvailability, AttendeeRole, AttendeeStatus, BusyStatus, CalendarEvent, CalendarId, EmailAddress, EventAttachment, EventOccurrence, EventSensitivity,
    EventStatus, EventUid, FreeBusyInterval, RecurrenceRange, MAX_INLINE_EVENT_ATTACHMENT_BYTES,
};

use crate::calendar_colors::CalendarColorMap;
use crate::calendar_tz::DisplayZone;
use crate::datetime_picker::{set_calendar_date, time_cluster, wire_calendar};
use crate::recipient_entry::{address_of, RecipientEntry, SuggestionSource};
use crate::recurrence;
use crate::schedule_grid::{self, ScheduleRow};

/// The editor's "Show as" choices in new Outlook's order (CAL-05). The
/// index↔state mapping is explicit - never the enum's own discriminant - so
/// reordering `BusyStatus` can't silently change what a picker index means.
/// Outlook calls the OOF state "Away" in new Outlook.
const SHOW_AS_CHOICES: [(BusyStatus, &str); 5] = [
    (BusyStatus::Free, "Free"),
    (BusyStatus::WorkingElsewhere, "Working elsewhere"),
    (BusyStatus::Tentative, "Tentative"),
    (BusyStatus::Busy, "Busy"),
    (BusyStatus::Oof, "Away"),
];

/// Microsoft Graph's per-attachment limit for an event attachment
/// (`POST /me/events/{id}/attachments`). Larger files need an upload session,
/// which isn't implemented, so the editor refuses them up front instead of
/// letting the server reject the meeting's file.
const GRAPH_ATTACHMENT_LIMIT_BYTES: u64 = 3 * 1024 * 1024;

/// The dropdown index showing `show_as` (unknown states fall back to Busy).
fn show_as_index(show_as: BusyStatus) -> u32 {
    match SHOW_AS_CHOICES.iter().position(|(choice, _)| *choice == show_as) {
        Some(index) => index as u32,
        None => SHOW_AS_CHOICES.iter().position(|(choice, _)| *choice == BusyStatus::Busy).expect("Busy is always offered") as u32,
    }
}

/// The state a dropdown index means (defaulting to Busy outside the list).
fn show_as_from_index(index: u32) -> BusyStatus {
    SHOW_AS_CHOICES.get(index as usize).map(|(choice, _)| *choice).unwrap_or(BusyStatus::Busy)
}

/// The save callback's target: the picked calendar plus the finished event.
pub type SaveCallback = Rc<dyn Fn(CalendarId, CalendarEvent)>;
/// The delete callback's target: the calendar plus the occurrence being
/// deleted - the whole occurrence, so the caller can decide whether to
/// delete the resource outright (an override) or EXDATE the instance out of
/// its master (a plain expansion of a recurring series).
type DeleteCallback = Rc<dyn Fn(CalendarId, EventOccurrence)>;
/// The forward callback (CAL-18): no arguments - the caller closed over the
/// occurrence it builds the forward from. Fired by the top bar's Forward
/// button, shown only for a meeting someone else organizes.
pub type ForwardCallback = Rc<dyn Fn()>;
/// The edited occurrence's identity + write target, threaded through the form.
type EventMeta = (EventUid, Option<String>, Option<String>);

/// A free/busy request the Scheduling page makes to its caller (CAL-19):
/// which calendar's account to ask, the attendees, and the UTC window to
/// cover. `generation` increases with each request so a late answer for an
/// older one is discarded.
pub struct AvailabilityRequest {
    pub generation: u64,
    pub calendar_id: CalendarId,
    pub attendees: Vec<EmailAddress>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// The caller's half of the scheduling bridge: it routes `AvailabilityRequest`s
/// to the owning account's session and pushes answers back through the
/// [`SchedulingHandle`] (keyed by the request's generation).
#[derive(Clone)]
pub struct SchedulingContext {
    pub handle: SchedulingHandle,
    pub request: Rc<dyn Fn(AvailabilityRequest)>,
}

/// The listener an open editor installs to receive free/busy answers.
type AvailabilityListener = Rc<dyn Fn(u64, Vec<AttendeeAvailability>, Option<String>)>;
/// A repaint hook shared between the Scheduling page's closures.
type RefreshHook = Rc<dyn Fn()>;

/// The handle the caller uses to deliver free/busy answers to an open editor.
/// The editor installs a listener; the caller calls [`Self::deliver`] /
/// [`Self::fail`] as events arrive. Data-in/out, so the dialog never touches
/// the network itself.
#[derive(Clone)]
pub struct SchedulingHandle {
    listener: Rc<RefCell<Option<AvailabilityListener>>>,
}

impl Default for SchedulingHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulingHandle {
    pub fn new() -> Self {
        SchedulingHandle {
            listener: Rc::new(RefCell::new(None)),
        }
    }

    /// Registers the editor's listener (replacing any previous one - the
    /// editor is modal, so only one is ever open).
    pub fn set_listener(&self, listener: impl Fn(u64, Vec<AttendeeAvailability>, Option<String>) + 'static) {
        *self.listener.borrow_mut() = Some(Rc::new(listener));
    }

    /// Delivers a free/busy answer for `generation`.
    pub fn deliver(&self, generation: u64, availabilities: Vec<AttendeeAvailability>) {
        if let Some(listener) = self.listener.borrow().as_ref() {
            listener(generation, availabilities, None);
        }
    }

    /// Reports that a lookup couldn't be answered (no backend, server error).
    pub fn fail(&self, generation: u64, message: String) {
        if let Some(listener) = self.listener.borrow().as_ref() {
            listener(generation, Vec::new(), Some(message));
        }
    }
}

/// Fetches one non-inline (server-managed or URI) attachment through the
/// owning calendar session (CAL-21). The editor's rows call this when the user
/// clicks Save on an attachment whose bytes aren't in the event document; the
/// window supplies an implementation backed by `request_event_attachment`.
pub type AttachmentFetchCallback = Rc<dyn Fn(CalendarId, Option<String>, EventAttachment, Rc<dyn Fn(String, Vec<u8>)>, Rc<dyn Fn(String)>)>;

/// What to prefill the editor with: the pickable calendars (label + id), which
/// calendar is selected by default, the event being edited (if any), and a
/// read-only snapshot for the right-hand mini-calendar/day-strip preview.
pub struct EventEditorPrefill<'a> {
    /// `(display label, calendar id)` for the calendar picker, in picker
    /// order. Empty disables the picker (there's no writable calendar).
    pub calendars: &'a [(String, CalendarId)],
    pub default_calendar: CalendarId,
    /// The occurrence being edited; `None` opens the blank "new event" form.
    /// While set, the calendar picker is locked to the event's own calendar
    /// (moving an event between calendars is out of scope for this pass).
    pub existing: Option<&'a EventOccurrence>,
    /// For a new event, the suggested start (local naive time) - typically the
    /// date the caller's view is anchored on, rounded to a whole hour. `None`
    /// falls back to the editor's own "next whole hour" default.
    pub suggested_start: Option<NaiveDateTime>,
    /// For a new event, the suggested end (local naive time) - the tail of a
    /// highlighted time range the user selected in the main grid. `None`
    /// defaults to one hour after `suggested_start`.
    pub suggested_end: Option<NaiveDateTime>,
    /// For a new event, a subject to prefill - the message menu's "Reply with
    /// meeting" (item 3.19 / C-20) passes the conversation's subject. Ignored
    /// when editing an existing occurrence (whose own summary wins), and when
    /// blank.
    pub suggested_subject: Option<&'a str>,
    /// Every currently-cached occurrence (from checked calendars) for
    /// whatever month the caller had loaded - used only to populate the
    /// read-only preview panel, re-sliced by day locally as the user
    /// navigates the mini-calendar or changes the start date. The dialog has
    /// no network access, so a day/month outside this snapshot just renders
    /// an empty strip rather than fetching anything.
    pub month_occurrences: &'a [EventOccurrence],
    /// Which local dates in `month_occurrences`' month have at least one
    /// occurrence - feeds the preview mini-calendar's bold event-day markers.
    pub month_event_days: &'a HashSet<NaiveDate>,
    pub calendar_colors: &'a CalendarColorMap,
    /// Best-effort address for the owning account, used as `ORGANIZER` only
    /// when the event ends up with at least one attendee. `None` when it
    /// couldn't be derived - `ATTENDEE`s are still written without an
    /// `ORGANIZER` in that case (a documented non-conformance, not a blocker).
    pub owner_email: Option<String>,
    /// View-only mode for events from read-only calendars (webcal feeds and
    /// the synthesized birthdays calendar have no write-back path): every
    /// input is disabled, the save/delete actions are hidden, and a dim note
    /// explains why. Requires `existing`.
    pub read_only: bool,
    /// The dim note shown under the form when `read_only` - why this
    /// calendar can't be written to (the source differs: feeds vs.
    /// synthesized birthdays). Ignored when not read-only.
    pub read_only_note: &'a str,
    /// The Calendar preferences (item 3.1): the preview's week start, work
    /// hours and second time zone, the recurrence weekday order, and the
    /// default reminder for a new event.
    pub calendar_prefs: &'a crate::settings::CalendarPreferences,
    /// Calendar ids served by the Microsoft Graph backend. Graph's recurrence
    /// model can't express every RRULE this editor builds (notably the
    /// `BYSETPOS` "last weekday" form), so Save refuses those with an inline
    /// message instead of letting the server silently drop the recurrence
    /// (item 3.10 / CAL-13).
    pub graph_calendars: &'a HashSet<CalendarId>,
    /// The shared category master list (item 2.1 / CAL-07), offered by the
    /// editor's Categorize picker. Empty when the user has none yet.
    pub categories: &'a [crate::categories::CategoryDef],
    /// Attendees to prefill on a *new* event - the profile card's "Schedule
    /// meeting" action passes the person it is about. Ignored when editing an
    /// existing occurrence (whose own attendees win).
    pub suggested_attendees: &'a [EmailAddress],
    /// The Scheduling Assistant bridge (item 3.8 / CAL-19): when present, the
    /// toolbar offers a Scheduling tab that asks the caller for attendees'
    /// free/busy. `None` (no calendar account able to answer) hides the tab.
    pub scheduling: Option<&'a SchedulingContext>,
    /// The attachment download bridge (item 3.16 / CAL-21): when present, a
    /// listed attachment that isn't inline can be saved through the owning
    /// calendar session. `None` leaves such rows display-only.
    pub attachment_fetch: Option<AttachmentFetchCallback>,
    /// The room-finder bridge (item 3.8 / CAL-19): when present, the Location
    /// row offers Microsoft 365's bookable rooms for a Graph-backed calendar.
    /// `None` hides the action entirely (no Graph account, or a CalDAV-only
    /// setup).
    pub rooms: Option<&'a crate::room_finder::RoomFinder>,
}

/// Opens the event editor as a modal dialog. `on_save` fires with the chosen
/// calendar and the finished event (new or edited); `on_delete` fires with the
/// delete target; `on_forward` - when the edited event is a meeting someone
/// else organizes (CAL-18) - fires from the top bar's Forward button. All are
/// responsible for routing to the owning account's session - the dialog only
/// builds and validates the event.
pub fn show_event_editor(
    window: &adw::ApplicationWindow,
    prefill: EventEditorPrefill,
    attendee_suggestions: SuggestionSource,
    profile_card: crate::profile_card::ContextProvider,
    on_save: impl Fn(CalendarId, CalendarEvent) + 'static,
    on_delete: impl Fn(CalendarId, EventOccurrence) + 'static,
    on_forward: Option<ForwardCallback>,
) {
    // Boxed as trait objects: a closure capturing a generic `impl Fn`
    // parameter can't implement `Fn` itself (its own `call` needs the captured
    // generic's bound to line up with a late-bound lifetime), so `Rc<dyn Fn>`
    // gives the handler closures a concrete, nameable type to capture.
    let on_save: SaveCallback = Rc::new(on_save);
    let on_delete: DeleteCallback = Rc::new(on_delete);

    let existing: Option<EventOccurrence> = prefill.existing.cloned();
    let has_existing = existing.is_some();
    let read_only = prefill.read_only;

    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title(if read_only {
            "Event"
        } else if has_existing {
            "Edit event"
        } else {
            "New event"
        })
        .default_width(920)
        .default_height(640)
        .build();

    // --- Calendar picker: a `StringList` of labels plus a parallel id vector,
    // so the selected index maps straight back to a `CalendarId`.
    let calendar_labels: Vec<String> = prefill.calendars.iter().map(|(label, _)| label.clone()).collect();
    let calendar_ids: Vec<CalendarId> = prefill.calendars.iter().map(|(_, id)| id.clone()).collect();
    let default_index = calendar_ids.iter().position(|id| *id == prefill.default_calendar).unwrap_or(0) as u32;
    let label_refs: Vec<&str> = calendar_labels.iter().map(String::as_str).collect();
    let string_list = gtk::StringList::new(&label_refs);
    // A plain `gtk::DropDown` rather than `adw::ComboRow` - `ComboRow`
    // reserves a fixed-width value label next to its title and ellipsizes
    // whatever doesn't fit, which clips the "Account · Calendar" labels here
    // whenever they're long. `DropDown`'s button and popup both size to their
    // widest item's natural width instead, so nothing gets clipped.
    let calendar_dropdown = gtk::DropDown::builder()
        .model(&string_list)
        .selected(if calendar_ids.is_empty() { u32::MAX } else { default_index })
        .sensitive(!calendar_ids.is_empty() && !has_existing)
        .valign(gtk::Align::Center)
        .build();
    // Calendar names are content: blurred by privacy mode.
    crate::privacy::mark_content(&calendar_dropdown);
    let calendar_row = adw::ActionRow::builder().title("Calendar").build();
    calendar_row.add_suffix(&calendar_dropdown);

    // --- Title / attendees / all-day toggle.
    let title_row = adw::EntryRow::builder().title("Title").build();
    let attendees_field = RecipientEntry::new("Invite required attendees");
    attendees_field.set_suggestion_source(attendee_suggestions);
    // Each attendee chip opens the person's profile card (Phase 4.2 / UX-67).
    {
        let provider = profile_card.clone();
        attendees_field.set_person_card_hook(Rc::new(move |label, address| {
            let address = address.clone();
            let resolve: crate::profile_card::PersonResolver = Rc::new(move || Some((address.clone(), None)));
            crate::profile_card::attach_person_trigger(provider.clone(), label, true, resolve);
        }));
    }
    let all_day_switch = gtk::Switch::new();
    all_day_switch.set_valign(gtk::Align::Center);

    // --- Start/end date + time. Each date is tracked through a
    // `Rc<Cell<NaiveDate>>` updated on `day-selected` (GtkCalendar's getters
    // need v4_14, which this crate builds against); times are two spin buttons
    // each, insensitive while the all-day toggle is on.
    let start_date = Rc::new(Cell::new(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()));
    let end_date = Rc::new(Cell::new(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()));
    let start_calendar = gtk::Calendar::new();
    let end_calendar = gtk::Calendar::new();
    let start_hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
    let start_minute = gtk::SpinButton::with_range(0.0, 59.0, 1.0);
    let end_hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
    let end_minute = gtk::SpinButton::with_range(0.0, 59.0, 1.0);
    for spin in [&start_hour, &start_minute, &end_hour, &end_minute] {
        spin.set_digits(0);
        spin.set_width_chars(2);
    }

    // The initial span comes from the edited event, the caller's suggested
    // start (for a new event), or a sensible fresh default (next whole hour,
    // one hour long). A recurring event is prefilled from the series *anchor*
    // (master DTSTART/DTEND), not the clicked occurrence's expansion, so a
    // metadata-only edit can't silently re-anchor the whole series. An
    // occurrence that *is* a per-occurrence override is the exception: its
    // own times are the override's, and prefilling the master anchor would
    // re-anchor the override to the series time.
    let existing_all_day = existing.as_ref().map(|occ| occ.all_day).unwrap_or(false);
    // CAL-11: an event with its own time zone pre-fills (and saves) its
    // wall-clock fields in that zone; one without follows the calendar's
    // display zone.
    let event_display_zone = existing
        .as_ref()
        .and_then(|occ| occ.time_zone.as_deref())
        .and_then(|name| name.parse::<chrono_tz::Tz>().ok())
        .map(crate::calendar_tz::DisplayZone::Named)
        .unwrap_or_else(crate::calendar_tz::display_zone);
    let (initial_start, initial_end) = existing
        .as_ref()
        .map(|occ| {
            if occ.recurrence_id.is_none() {
                if let (Some(master_start), Some(master_end)) = (occ.master_start, occ.master_end) {
                    return crate::calendar_tz::occurrence_span(event_display_zone, master_start, master_end, occ.all_day);
                }
            }
            crate::calendar_tz::occurrence_span(event_display_zone, occ.start, occ.end, occ.all_day)
        })
        .or_else(|| {
            prefill
                .suggested_start
                .map(|start| (start, prefill.suggested_end.unwrap_or(start + chrono::Duration::hours(1))))
        })
        .unwrap_or_else(default_event_times);
    // The form's all-day end is the *last* day (inclusive, Outlook/Gmail
    // convention); the model stores the exclusive day after, so show one less.
    let initial_end = if existing_all_day { initial_end - chrono::Duration::days(1) } else { initial_end };
    set_calendar_date(&start_calendar, &start_date, initial_start.date());
    set_calendar_date(&end_calendar, &end_date, initial_end.date());
    wire_calendar(&start_calendar, &start_date);
    wire_calendar(&end_calendar, &end_date);
    start_hour.set_value(f64::from(initial_start.hour()));
    start_minute.set_value(f64::from(initial_start.minute()));
    end_hour.set_value(f64::from(initial_end.hour()));
    end_minute.set_value(f64::from(initial_end.minute()));

    all_day_switch.set_active(existing_all_day);
    for spin in [&start_hour, &start_minute, &end_hour, &end_minute] {
        spin.set_sensitive(!existing_all_day);
    }
    // CAL-11: the event's own time zone, in the date/time row (UX-59). The
    // picker lists every IANA zone; an all-day event has no zone, so it goes
    // insensitive with the time spins.
    let zone_model = gtk::StringList::new(crate::calendar_tz::iana_zone_names());
    let zone_dropdown = gtk::DropDown::builder().model(&zone_model).enable_search(true).tooltip_text("Time zone").build();
    zone_dropdown.set_selected(initial_zone_index(existing.as_ref().and_then(|occ| occ.time_zone.as_deref())));
    zone_dropdown.set_sensitive(!existing_all_day);
    {
        let start_date = start_date.clone();
        let end_date = end_date.clone();
        let start_hour = start_hour.clone();
        let start_minute = start_minute.clone();
        let end_hour = end_hour.clone();
        let end_minute = end_minute.clone();
        let end_calendar = end_calendar.clone();
        let zone_dropdown = zone_dropdown.clone();
        all_day_switch.connect_active_notify(move |switch| {
            let on = switch.is_active();
            zone_dropdown.set_sensitive(!on);
            for spin in [&start_hour, &start_minute, &end_hour, &end_minute] {
                spin.set_sensitive(!on);
            }
            if !on {
                // Coming off all-day, the times may no longer form a valid
                // span - a same-day all-day event prefills both ends at 00:00.
                // Nudge the end forward an hour rather than letting Save fail.
                let start = start_date.get().and_hms_opt(start_hour.value() as u32, start_minute.value() as u32, 0).unwrap();
                let end = end_date.get().and_hms_opt(end_hour.value() as u32, end_minute.value() as u32, 0).unwrap();
                if end <= start {
                    let bumped = start + chrono::Duration::hours(1);
                    end_hour.set_value(f64::from(bumped.hour()));
                    end_minute.set_value(f64::from(bumped.minute()));
                    if bumped.date() != end_date.get() {
                        set_calendar_date(&end_calendar, &end_date, bumped.date());
                    }
                }
            }
        });
    }

    let datetime_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    datetime_row.append(&time_cluster("Starts", &start_calendar, &start_hour, &start_minute));
    let to_label = gtk::Label::builder().label("to").css_classes(["dim-label"]).build();
    crate::privacy::mark_ui(&to_label);
    datetime_row.append(&to_label);
    datetime_row.append(&time_cluster("Ends", &end_calendar, &end_hour, &end_minute));
    let all_day_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .hexpand(true)
        .valign(gtk::Align::Start)
        .build();
    let all_day_label = gtk::Label::new(Some("All day"));
    crate::privacy::mark_ui(&all_day_label);
    all_day_box.append(&all_day_label);
    all_day_box.append(&all_day_switch);
    datetime_row.append(&all_day_box);
    let zone_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).valign(gtk::Align::Start).build();
    let time_zone_label = gtk::Label::builder().label("Time zone").css_classes(["dim-label", "caption"]).xalign(0.0).build();
    crate::privacy::mark_ui(&time_zone_label);
    zone_box.append(&time_zone_label);
    zone_box.append(&zone_dropdown);
    datetime_row.append(&zone_box);

    // --- Location + video call + notes.
    let location_row = adw::EntryRow::builder().title("Location").build();
    // Room finder (CAL-19): Microsoft 365's tenant room directory with each
    // room's free/busy for the meeting window. The button rides the Location
    // row as a suffix and is shown only for a Graph-backed calendar - CalDAV
    // and webcal events keep Outlook's free-text Location.
    let room_finder = prefill.rooms.map(|finder| {
        crate::room_finder::attach(crate::room_finder::RoomFinderInputs {
            location_row: &location_row,
            attendees_field: &attendees_field,
            calendar_dropdown: &calendar_dropdown,
            calendar_ids: &calendar_ids,
            graph_calendars: prefill.graph_calendars,
            start_date: &start_date,
            end_date: &end_date,
            start_hour: &start_hour,
            start_minute: &start_minute,
            end_hour: &end_hour,
            end_minute: &end_minute,
            start_calendar: &start_calendar,
            end_calendar: &end_calendar,
            all_day_switch: &all_day_switch,
            zone_dropdown: &zone_dropdown,
            finder: finder.clone(),
        })
    });
    let resource_attendees = room_finder.as_ref().map(|widgets| widgets.resource_attendees.clone()).unwrap_or_default();
    let room_button = room_finder.map(|widgets| widgets.button);

    let video_toggle = adw::SwitchRow::builder().title("Video call link").subtitle("Add a join-this-meeting URL").build();
    let video_url_row = adw::EntryRow::builder().title("Meeting URL").build();
    video_url_row.set_visible(video_toggle.is_active());
    // CAL-20 / 3.17: on a Microsoft 365 calendar this is Outlook's "Teams
    // meeting" toggle - Graph creates the join link on save, so there is no
    // URL to type. Other calendars keep the manual join-link row, whose
    // subtitle names the provider once a recognised URL is entered.
    let sync_video_row: Rc<dyn Fn()> = {
        let video_toggle = video_toggle.clone();
        let video_url_row = video_url_row.clone();
        let calendar_dropdown = calendar_dropdown.clone();
        let calendar_ids = calendar_ids.clone();
        let graph_calendars = prefill.graph_calendars.clone();
        Rc::new(move || {
            let graph = calendar_ids
                .get(calendar_dropdown.selected() as usize)
                .is_some_and(|calendar_id| graph_calendars.contains(calendar_id));
            if graph {
                video_toggle.set_title("Teams meeting");
                video_toggle.set_subtitle("Microsoft Teams adds a join link when you save");
                video_url_row.set_visible(false);
            } else {
                video_toggle.set_title("Video call link");
                video_toggle.set_subtitle(&match lookout_core::interop::onlinemeeting::classify(video_url_row.text().trim()) {
                    Some(provider) => format!("{} meeting link", provider.display_name()),
                    None => "Add a join-this-meeting URL".to_string(),
                });
                video_url_row.set_visible(video_toggle.is_active());
            }
        })
    };
    {
        let sync_video_row = sync_video_row.clone();
        video_toggle.connect_active_notify(move |_| sync_video_row());
    }
    {
        let sync_video_row = sync_video_row.clone();
        video_url_row.connect_changed(move |_| sync_video_row());
    }
    {
        let sync_video_row = sync_video_row.clone();
        calendar_dropdown.connect_selected_notify(move |_| sync_video_row());
    }

    let description_buffer = gtk::TextBuffer::new(None);
    let description_view = gtk::TextView::builder()
        .buffer(&description_buffer)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(6)
        .left_margin(6)
        .right_margin(6)
        .build();

    // --- Form layout: a preferences-style group for the row-shaped fields,
    // the compact date/time row, and the notes body filling the rest.
    let fields_group = adw::PreferencesGroup::new();
    fields_group.add(&title_row);
    fields_group.add(attendees_field.widget());
    fields_group.add(&calendar_row);
    fields_group.add(&location_row);
    fields_group.add(&video_toggle);
    fields_group.add(&video_url_row);
    // CAL-21: the event's files - managed/URI attachments already on the
    // event, plus anything the user attaches here. A read-only (webcal) event
    // still lists and saves what it carries, but offers no add/remove; a
    // server-held file there has no calendar session to download it from, so
    // only inline contents get a Save button.
    let attachment_fetch = if read_only { None } else { prefill.attachment_fetch.clone() };
    let attachment_calendar = existing.as_ref().map(|occ| occ.calendar_id.clone()).unwrap_or_else(|| prefill.default_calendar.clone());
    let (attachments_group, attachments) = build_event_attachments_group(
        existing.as_ref().map(|occ| occ.attachments.clone()).unwrap_or_default(),
        read_only,
        attachment_calendar,
        existing.as_ref().and_then(|occ| occ.href.clone()),
        attachment_fetch,
    );

    let recurring_note = gtk::Label::builder()
        .label("This event repeats - changes apply to the whole series.")
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    recurring_note.set_visible(existing.as_ref().is_some_and(|occ| occ.rrule.is_some()));
    crate::privacy::mark_ui(&recurring_note);
    let read_only_note = gtk::Label::builder()
        .label(prefill.read_only_note)
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    read_only_note.set_visible(read_only);
    crate::privacy::mark_ui(&read_only_note);
    let error_label = gtk::Label::builder().wrap(true).xalign(0.0).halign(gtk::Align::Start).css_classes(["error"]).build();
    crate::privacy::mark_ui(&error_label);
    error_label.set_visible(false);

    let notes_label = gtk::Label::builder()
        .label("Notes")
        .css_classes(["dim-label", "caption"])
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    crate::privacy::mark_ui(&notes_label);
    let notes_scroller = gtk::ScrolledWindow::builder()
        .child(&description_view)
        .vexpand(true)
        .min_content_height(120)
        .css_classes(["card"])
        .build();

    let form_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    form_box.append(&fields_group);
    form_box.append(&attachments_group);
    form_box.append(&datetime_row);
    form_box.append(&read_only_note);
    form_box.append(&recurring_note);
    form_box.append(&error_label);
    form_box.append(&notes_label);
    form_box.append(&notes_scroller);
    let form_scroller = gtk::ScrolledWindow::builder()
        .child(&form_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .hexpand(true)
        .vexpand(true)
        .build();

    // --- Right-hand preview: a mini month calendar above a single-day
    // schedule strip, reusing the widgets the main Calendar tab already
    // builds for itself (`calendar_view::build_mini`/`build_time_grid`).
    let preview_mini = crate::calendar_view::build_mini(prefill.calendar_prefs.week_start);
    let preview_day_strip = Rc::new(crate::calendar_view::build_time_grid(
        &[prefill.calendar_prefs.week_start],
        true,
        &Rc::new(RefCell::new(None)),
    ));
    crate::calendar_view::apply_time_grid_preferences(&preview_day_strip, prefill.calendar_prefs);
    let preview_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .width_request(280)
        .margin_top(12)
        .margin_bottom(12)
        .margin_end(12)
        .build();
    preview_box.append(&preview_mini.root);
    preview_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    preview_box.append(&preview_day_strip.root);

    let main_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&form_scroller)
        .end_child(&preview_box)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(620)
        .css_classes(["seamless-paned"])
        .build();

    // --- The Tracking page (item 3.6 / CAL-16): Outlook's per-attendee
    // response list, swapped in by the top bar's Tracking toggle. Only a
    // meeting that already has invitees has anything to track.
    let editor_stack = gtk::Stack::new();
    editor_stack.add_named(&main_paned, Some("form"));
    let has_tracking = existing.as_ref().is_some_and(|occ| !occ.attendees.is_empty());
    if let Some(occ) = existing.as_ref().filter(|occ| !occ.attendees.is_empty()) {
        let tracking_page = build_tracking_page(occ, prefill.owner_email.as_deref());
        editor_stack.add_named(&tracking_page, Some("tracking"));
    }
    editor_stack.set_visible_child_name("form");

    // --- Top action row: Cancel/Save plus the toolbar-style controls the
    // reference layout groups above the fields (Series, Busy, Categorize,
    // Reminder, Sensitivity), with Print/Options as disabled placeholders -
    // the same `set_sensitive(false)` convention the main calendar toolbar
    // already uses for controls with no backend behind them yet.
    let cancel_button = gtk::Button::with_label("Cancel");
    let save_button = gtk::Button::with_label(save_button_label(has_existing, !attendees_field.addresses().is_empty()));
    save_button.add_css_class("suggested-action");
    // UX-59: the footer verb follows the form - an appointment saves, a
    // meeting with attendees sends (Outlook's own switch). Kept in step live
    // through the attendees field's change callback.
    {
        let save_button = save_button.clone();
        let label_field = attendees_field.clone();
        attendees_field.connect_addresses_changed(move || {
            save_button.set_label(save_button_label(has_existing, !label_field.addresses().is_empty()));
        });
    }

    let series = recurrence::build_series_control(
        existing.as_ref().and_then(|occ| occ.rrule.as_deref()),
        prefill.calendar_prefs.week_start,
        initial_start.date(),
    );
    series.button.set_tooltip_text(Some("Series"));

    let busy_model = gtk::StringList::new(&SHOW_AS_CHOICES.iter().map(|(_, label)| *label).collect::<Vec<_>>());
    let busy_dropdown = gtk::DropDown::builder().model(&busy_model).tooltip_text("Show as").build();
    busy_dropdown.set_selected(show_as_index(existing.as_ref().map(|occ| occ.show_as).unwrap_or_default()));

    // Categorize (item 2.1 / CAL-07): the shared master list as toggle rows,
    // plus any category the event already carries that isn't in the list (a
    // name written by another client), so nothing is silently dropped.
    let (categorize_button, selected_categories) = build_category_picker(prefill.categories, existing.as_ref().map(|occ| occ.categories.as_slice()).unwrap_or(&[]), read_only);
    categorize_button.set_tooltip_text(Some("Categorize"));

    let reminder_choices = crate::settings::REMINDER_CHOICES;
    let reminder_model = gtk::StringList::new(&reminder_choices.iter().map(|(label, _)| *label).collect::<Vec<_>>());
    let reminder_dropdown = gtk::DropDown::builder().model(&reminder_model).tooltip_text("Reminder").build();
    let reminder_index = initial_reminder_index(existing.as_ref().map(|occ| occ.reminder_minutes_before), prefill.calendar_prefs.default_reminder_minutes);
    reminder_dropdown.set_selected(reminder_index as u32);

    let sensitivity_model = gtk::StringList::new(&["Public", "Private", "Confidential"]);
    let sensitivity_dropdown = gtk::DropDown::builder().model(&sensitivity_model).tooltip_text("Sensitivity").build();
    sensitivity_dropdown.set_selected(existing.as_ref().map(|occ| occ.sensitivity).unwrap_or_default() as u32);

    let options_button = gtk::Button::from_icon_name(themed_icon("view-grid-symbolic", &["open-menu-symbolic"]));
    options_button.set_tooltip_text(Some("More options"));
    options_button.set_sensitive(false);
    // Print snapshots the form's current state as HTML and sends it through
    // WebKit's print pipeline - useful for read-only (webcal) events too,
    // which have no other way out of the dialog.
    let print_button = gtk::Button::from_icon_name("printer-symbolic");
    print_button.set_tooltip_text(Some("Print"));

    // Tracking (item 3.6 / CAL-16): a toggle that swaps the form/preview for
    // the per-attendee response list. Display-only, so it stays usable on a
    // read-only event; hidden when there are no invitees to track.
    let tracking_button = gtk::ToggleButton::builder().label("Tracking").tooltip_text("Tracking").valign(gtk::Align::Center).build();
    tracking_button.set_visible(has_tracking);
    {
        let editor_stack = editor_stack.clone();
        tracking_button.connect_toggled(move |button| {
            editor_stack.set_visible_child_name(if button.is_active() { "tracking" } else { "form" });
        });
    }

    let delete_button = gtk::Button::from_icon_name(themed_icon("user-trash-symbolic", &["edit-delete-symbolic"]));
    delete_button.add_css_class("destructive-action");
    delete_button.set_tooltip_text(Some("Delete"));
    delete_button.set_visible(has_existing);

    // Forward (CAL-18): a meeting someone else organizes can be forwarded to
    // new recipients, exactly as Outlook's meeting window offers. Shown only
    // when there are attendees, an organizer to notify, and the callback -
    // the writeable-calendar case; a read-only feed has no sender.
    let forward_visible = !read_only
        && existing.as_ref().is_some_and(|occ| {
            !occ.attendees.is_empty()
                && occ
                    .organizer
                    .as_ref()
                    .is_some_and(|organizer| prefill.owner_email.as_deref().is_none_or(|owner| !owner.eq_ignore_ascii_case(&organizer.address)))
        });
    let forward_button = gtk::Button::builder().label("Forward").tooltip_text("Forward meeting").valign(gtk::Align::Center).build();
    forward_button.set_visible(forward_visible && on_forward.is_some());
    if let Some(on_forward) = on_forward {
        forward_button.connect_clicked(move |_| on_forward());
    }

    let top_bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    top_bar.append(&cancel_button);
    top_bar.append(&save_button);
    top_bar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    top_bar.append(&series.button);
    // The recurring-edit scope: "This occurrence" (a per-occurrence override),
    // "This and following" (an override with RANGE=THISANDFUTURE), or "All
    // events" (rewrite the master). Only meaningful for a recurring event -
    // hidden otherwise, and defaulting to the pre-scope behavior ("All
    // events") so a plain edit keeps doing exactly what it always did.
    let is_recurring = existing.as_ref().is_some_and(|occ| occ.rrule.is_some() || occ.recurrence_id.is_some());
    let scope_model = gtk::StringList::new(&["This occurrence", "This and following", "All events"]);
    let scope_dropdown = gtk::DropDown::builder().model(&scope_model).tooltip_text("Edit scope").build();
    scope_dropdown.set_selected(2);
    scope_dropdown.set_visible(is_recurring);
    top_bar.append(&scope_dropdown);
    top_bar.append(&busy_dropdown);
    top_bar.append(&categorize_button);
    top_bar.append(&reminder_dropdown);
    top_bar.append(&sensitivity_dropdown);
    top_bar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    top_bar.append(&tracking_button);
    top_bar.append(&forward_button);
    top_bar.append(&options_button);
    top_bar.append(&print_button);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    top_bar.append(&spacer);
    top_bar.append(&delete_button);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&top_bar);
    toolbar_view.set_content(Some(&editor_stack));
    dialog.set_content(Some(&toolbar_view));

    // --- Prefill text fields from the edited occurrence.
    if let Some(occ) = &existing {
        title_row.set_text(occ.summary.as_deref().unwrap_or(""));
        location_row.set_text(occ.location.as_deref().unwrap_or(""));
        if let Some(description) = &occ.description {
            description_buffer.set_text(description);
        }
        if !occ.attendees.is_empty() {
            attendees_field.set_from_text(&occ.attendees.iter().map(attendee_to_token).collect::<Vec<_>>().join(", "));
        }
        video_toggle.set_active(occ.conference_url.is_some());
        video_url_row.set_text(occ.conference_url.as_deref().unwrap_or(""));
    } else {
        if let Some(subject) = prefill.suggested_subject.map(str::trim).filter(|subject| !subject.is_empty()) {
            title_row.set_text(subject);
        }
        if !prefill.suggested_attendees.is_empty() {
            attendees_field.set_from_text(
                &prefill
                    .suggested_attendees
                    .iter()
                    .map(|address| match &address.name {
                        Some(name) if !name.trim().is_empty() => format!("{name} <{}>", address.address),
                        _ => address.address.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }
    // The form may default to a Microsoft 365 calendar, where the switch is
    // the Teams-meeting toggle rather than a manual join link.
    sync_video_row();

    // --- Right-hand preview wiring: re-renders the mini-calendar + day strip
    // from the caller's snapshot every time the form's date/time/title/
    // calendar changes, plus a synthetic chip for the event being edited
    // itself (which isn't - or is stale - in that snapshot).
    let displayed_day = Rc::new(Cell::new(initial_start.date()));
    let month_occurrences: Vec<EventOccurrence> = prefill.month_occurrences.to_vec();
    let month_event_days = prefill.month_event_days.clone();
    let calendar_colors = prefill.calendar_colors.clone();
    let refresh_preview: Rc<dyn Fn()> = {
        let start_date = start_date.clone();
        let start_hour = start_hour.clone();
        let start_minute = start_minute.clone();
        let end_date = end_date.clone();
        let end_hour = end_hour.clone();
        let end_minute = end_minute.clone();
        let all_day_switch = all_day_switch.clone();
        let title_row = title_row.clone();
        let calendar_dropdown = calendar_dropdown.clone();
        let calendar_ids = calendar_ids.clone();
        let preview_mini = preview_mini.clone();
        let preview_day_strip = preview_day_strip.clone();
        let displayed_day = displayed_day.clone();
        let existing = existing.clone();
        let zone_dropdown = zone_dropdown.clone();
        let month_occurrences = month_occurrences.clone();
        Rc::new(move || {
            let day = displayed_day.get();
            let all_day = all_day_switch.is_active();
            let zone = selected_zone(&zone_dropdown);
            let start_local = date_time_from_form(&start_date, &start_hour, &start_minute, all_day);
            let end_local_raw = date_time_from_form(&end_date, &end_hour, &end_minute, all_day);
            let model_end = if all_day { end_local_raw + chrono::Duration::days(1) } else { end_local_raw };
            let selected = calendar_dropdown.selected() as usize;
            let calendar_id = calendar_ids.get(selected).cloned().unwrap_or_else(|| CalendarId(String::new()));
            let synthetic = synthetic_occurrence(
                calendar_id,
                &title_row.text(),
                boundary_to_utc_in(zone, start_local, all_day),
                boundary_to_utc_in(zone, model_end, all_day),
                all_day,
            );
            let mut occs = occurrences_for_day(&month_occurrences, day, existing.as_ref());
            occs.push(synthetic);
            crate::calendar_view::set_mini_month(&preview_mini, day, &month_event_days);
            crate::calendar_view::set_time_grid(&preview_day_strip, day, &occs, &calendar_colors, None, None, None);
            crate::calendar_view::scroll_time_grid_to_minutes(&preview_day_strip, start_local.hour() as i64 * 60 + start_local.minute() as i64);
        })
    };
    {
        let refresh_preview = refresh_preview.clone();
        let displayed_day = displayed_day.clone();
        start_calendar.connect_day_selected(move |cal| {
            if let Some(date) = NaiveDate::from_ymd_opt(cal.year(), cal.month() as u32 + 1, cal.day() as u32) {
                displayed_day.set(date);
            }
            refresh_preview();
        });
    }
    {
        let refresh_preview = refresh_preview.clone();
        end_calendar.connect_day_selected(move |_| refresh_preview());
    }
    for spin in [&start_hour, &start_minute, &end_hour, &end_minute] {
        let refresh_preview = refresh_preview.clone();
        spin.connect_value_changed(move |_| refresh_preview());
    }
    {
        let refresh_preview = refresh_preview.clone();
        all_day_switch.connect_active_notify(move |_| refresh_preview());
    }
    {
        let refresh_preview = refresh_preview.clone();
        title_row.connect_changed(move |_| refresh_preview());
    }
    {
        let refresh_preview = refresh_preview.clone();
        calendar_dropdown.connect_selected_notify(move |_| refresh_preview());
    }
    {
        let refresh_preview = refresh_preview.clone();
        zone_dropdown.connect_selected_notify(move |_| refresh_preview());
    }
    {
        let refresh_preview = refresh_preview.clone();
        let displayed_day = displayed_day.clone();
        crate::calendar_view::connect_day_selected(&preview_mini, move |date| {
            displayed_day.set(date);
            refresh_preview();
        });
    }
    refresh_preview();

    // --- Scheduling Assistant page (item 3.8 / CAL-19): attendee rows over a
    // horizontal time grid, filled from the caller's free/busy bridge. Like
    // Tracking it is a named `editor_stack` child swapped in by a toolbar
    // toggle; unlike Tracking it must ask the caller for data (the dialog has
    // no network) and merges the answers in as they arrive.
    let scheduling_button: Option<gtk::ToggleButton> = if let Some(scheduling) = prefill.scheduling {
        let scheduling = scheduling.clone();
        let schedule_existing = existing.clone();
        let owner_subtitle = prefill.owner_email.clone();
        let work_start = prefill.calendar_prefs.work_start;
        let work_end = prefill.calendar_prefs.work_end;

        let grid = Rc::new(schedule_grid::build_schedule_grid(
            prefill.calendar_prefs,
            start_date.get(),
            zone_from_dropdown(&zone_dropdown),
        ));
        let availability: Rc<RefCell<Vec<AttendeeAvailability>>> = Rc::new(RefCell::new(Vec::new()));
        let loading = Rc::new(Cell::new(false));
        let generation = Rc::new(Cell::new(0u64));
        let refresh_rows_holder: Rc<RefCell<Option<RefreshHook>>> = Rc::new(RefCell::new(None));

        let suggestions_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let page_content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
        page_content.append(&grid.root);
        page_content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let suggested_heading = gtk::Label::builder()
            .label("Suggested times")
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .margin_start(12)
            .build();
        crate::privacy::mark_ui(&suggested_heading);
        page_content.append(&suggested_heading);
        page_content.append(&suggestions_box);
        let page = gtk::ScrolledWindow::builder()
            .child(&page_content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        editor_stack.add_named(&page, Some("scheduling"));

        // Sets the form's start/end from a picked slot (grid click or a
        // suggestion) and repaints the grid. The grid's date follows.
        let apply_time: Rc<dyn Fn(NaiveDateTime, NaiveDateTime)> = {
            let grid = grid.clone();
            let start_date = start_date.clone();
            let end_date = end_date.clone();
            let start_calendar = start_calendar.clone();
            let end_calendar = end_calendar.clone();
            let start_hour = start_hour.clone();
            let start_minute = start_minute.clone();
            let end_hour = end_hour.clone();
            let end_minute = end_minute.clone();
            let refresh_preview = refresh_preview.clone();
            let refresh_rows_holder = refresh_rows_holder.clone();
            Rc::new(move |start: NaiveDateTime, end: NaiveDateTime| {
                grid.set_date(start.date());
                set_calendar_date(&start_calendar, &start_date, start.date());
                start_hour.set_value(f64::from(start.hour()));
                start_minute.set_value(f64::from(start.minute()));
                set_calendar_date(&end_calendar, &end_date, end.date());
                end_hour.set_value(f64::from(end.hour()));
                end_minute.set_value(f64::from(end.minute()));
                refresh_preview();
                if let Some(refresh) = refresh_rows_holder.borrow().as_ref() {
                    refresh();
                }
            })
        };

        // Rebuilds the grid rows and the suggestion buttons from the form and
        // the latest availability answer.
        let refresh_rows: Rc<dyn Fn()> = {
            let grid = grid.clone();
            let availability = availability.clone();
            let loading = loading.clone();
            let attendees_field = attendees_field.clone();
            let zone_dropdown = zone_dropdown.clone();
            let start_date = start_date.clone();
            let end_date = end_date.clone();
            let start_hour = start_hour.clone();
            let start_minute = start_minute.clone();
            let end_hour = end_hour.clone();
            let end_minute = end_minute.clone();
            let all_day_switch = all_day_switch.clone();
            let month_occurrences = month_occurrences.clone();
            let suggestions_box = suggestions_box.clone();
            let apply_time = apply_time.clone();
            Rc::new(move || {
                let date = grid.date();
                let zone = zone_from_dropdown(&zone_dropdown);
                grid.set_zone(zone);
                let mut rows = vec![organizer_schedule_row(&month_occurrences, schedule_existing.as_ref(), date, owner_subtitle.clone())];
                let addresses = attendee_addresses(&attendees_field);
                let avail = availability.borrow();
                for address in &addresses {
                    let named = address.name.as_deref().map(str::trim).is_some_and(|name| !name.is_empty());
                    match avail.iter().find(|a| a.address.address.eq_ignore_ascii_case(&address.address)) {
                        Some(a) => rows.push(ScheduleRow {
                            label: address.display_label().to_string(),
                            subtitle: named.then(|| address.address.clone()),
                            intervals: a.intervals.clone(),
                            unknown: a.unknown,
                            loading: false,
                            organizer: false,
                        }),
                        None => rows.push(ScheduleRow {
                            label: address.display_label().to_string(),
                            subtitle: named.then(|| address.address.clone()),
                            intervals: Vec::new(),
                            unknown: !loading.get(),
                            loading: loading.get(),
                            organizer: false,
                        }),
                    }
                }
                drop(avail);
                grid.set_rows(rows);

                let all_day = all_day_switch.is_active();
                let start_local = date_time_from_form(&start_date, &start_hour, &start_minute, all_day);
                let end_local = date_time_from_form(&end_date, &end_hour, &end_minute, all_day);
                grid.set_proposed(Some(start_local), Some(end_local));

                // Suggested times: free slots across every attendee (the
                // organizer's own row is informational only).
                let duration = (end_local - start_local).num_minutes().max(30);
                let suggestions = schedule_grid::suggest_times(&availability.borrow(), date, work_start, work_end, zone, duration, 6);
                while let Some(child) = suggestions_box.first_child() {
                    suggestions_box.remove(&child);
                }
                if !addresses.is_empty() && !loading.get() && suggestions.is_empty() {
                    let no_free_time = gtk::Label::builder().label("No free time found on this day").xalign(0.0).css_classes(["dim-label"]).build();
                    crate::privacy::mark_ui(&no_free_time);
                    suggestions_box.append(&no_free_time);
                }
                for slot in suggestions {
                    let button = gtk::Button::with_label(&slot.format("%H:%M").to_string());
                    // A suggested time is calendar content: blurred by privacy mode.
                    crate::privacy::mark_content(&button);
                    button.set_tooltip_text(Some("Set the meeting to this time"));
                    let apply = apply_time.clone();
                    button.connect_clicked(move |_| apply(slot, slot + chrono::Duration::minutes(duration)));
                    suggestions_box.append(&button);
                }
            })
        };
        *refresh_rows_holder.borrow_mut() = Some(refresh_rows.clone());

        // Asks the caller for a fresh window over the current attendees.
        let request: Rc<dyn Fn()> = {
            let scheduling = scheduling.clone();
            let grid = grid.clone();
            let availability = availability.clone();
            let loading = loading.clone();
            let generation = generation.clone();
            let attendees_field = attendees_field.clone();
            let zone_dropdown = zone_dropdown.clone();
            let start_date = start_date.clone();
            let calendar_dropdown = calendar_dropdown.clone();
            let calendar_ids = calendar_ids.clone();
            let refresh_rows = refresh_rows.clone();
            Rc::new(move || {
                attendees_field.commit_pending();
                let addresses = attendee_addresses(&attendees_field);
                let next = generation.get() + 1;
                generation.set(next);
                availability.borrow_mut().clear();
                if addresses.is_empty() {
                    loading.set(false);
                    grid.set_status("Add attendees to check their availability.");
                    refresh_rows();
                    return;
                }
                loading.set(true);
                grid.set_status("Checking availability…");
                refresh_rows();
                // Query the whole day, so the suggested-times scan has the
                // full day's busy blocks (not just the current meeting span).
                let zone = selected_zone(&zone_dropdown);
                let day_start = start_date.get().and_hms_opt(0, 0, 0).unwrap();
                let start = boundary_to_utc_in(zone, day_start, false);
                let end = boundary_to_utc_in(zone, day_start + chrono::Duration::days(1), false);
                let calendar_id = calendar_ids
                    .get(calendar_dropdown.selected() as usize)
                    .cloned()
                    .unwrap_or_else(|| CalendarId(String::new()));
                (scheduling.request)(AvailabilityRequest {
                    generation: next,
                    calendar_id,
                    attendees: addresses,
                    start,
                    end,
                });
            })
        };

        // The caller pushes answers here (window.rs).
        {
            let grid = grid.clone();
            let availability = availability.clone();
            let loading = loading.clone();
            let generation = generation.clone();
            let refresh_rows = refresh_rows.clone();
            scheduling.handle.set_listener(move |answered, availabilities, error| {
                if answered != generation.get() {
                    return;
                }
                loading.set(false);
                match error {
                    Some(message) => {
                        availability.borrow_mut().clear();
                        grid.set_status(&format!("Couldn't check availability: {message}"));
                    }
                    None => {
                        *availability.borrow_mut() = availabilities;
                        grid.set_status("");
                    }
                }
                refresh_rows();
            });
        }

        // A grid click sets the start (keeping the current duration).
        {
            let apply_time = apply_time.clone();
            let all_day_switch = all_day_switch.clone();
            let start_date = start_date.clone();
            let end_date = end_date.clone();
            let start_hour = start_hour.clone();
            let start_minute = start_minute.clone();
            let end_hour = end_hour.clone();
            let end_minute = end_minute.clone();
            grid.connect_time_picked(move |start| {
                let all_day = all_day_switch.is_active();
                let current_start = date_time_from_form(&start_date, &start_hour, &start_minute, all_day);
                let current_end = date_time_from_form(&end_date, &end_hour, &end_minute, all_day);
                let duration = (current_end - current_start).num_minutes().max(30);
                apply_time(start, start + chrono::Duration::minutes(duration));
            });
        }

        // The grid's own date navigation moves the form (and vice versa).
        {
            let request = request.clone();
            let start_date = start_date.clone();
            let start_calendar = start_calendar.clone();
            let grid_for_cb = grid.clone();
            grid.connect_date_changed(move |date| {
                if grid_for_cb.date() != start_date.get() {
                    set_calendar_date(&start_calendar, &start_date, date);
                    request();
                }
            });
        }
        {
            let request = request.clone();
            let grid_for_cb = grid.clone();
            let start_date = start_date.clone();
            start_calendar.connect_day_selected(move |_| {
                let date = start_date.get();
                if grid_for_cb.date() != date {
                    grid_for_cb.set_date(date);
                    request();
                }
            });
        }

        let button = gtk::ToggleButton::builder()
            .label("Scheduling")
            .tooltip_text("Scheduling Assistant")
            .valign(gtk::Align::Center)
            .build();
        button.set_visible(!read_only);
        button.set_active(false);
        {
            let editor_stack = editor_stack.clone();
            let request = request.clone();
            let tracking_button = tracking_button.clone();
            let refresh_rows = refresh_rows.clone();
            button.connect_toggled(move |button| {
                if button.is_active() {
                    tracking_button.set_active(false);
                    editor_stack.set_visible_child_name("scheduling");
                    request();
                } else if !tracking_button.is_active() {
                    editor_stack.set_visible_child_name("form");
                }
                refresh_rows();
            });
        }
        {
            let button = button.clone();
            tracking_button.connect_toggled(move |tracking| {
                if tracking.is_active() {
                    button.set_active(false);
                }
            });
        }
        {
            let button = button.clone();
            let request = request.clone();
            attendees_field.connect_addresses_changed(move || {
                if button.is_active() {
                    request();
                }
            });
        }
        {
            let button = button.clone();
            let request = request.clone();
            zone_dropdown.connect_selected_notify(move |_| {
                if button.is_active() {
                    request();
                }
            });
        }
        {
            let request = request.clone();
            grid.connect_refresh(move || request());
        }
        Some(button)
    } else {
        None
    };
    if let Some(button) = &scheduling_button {
        top_bar.insert_child_after(button, Some(&tracking_button));
    }

    // --- Collect the form into a `CalendarEvent`. The uid/href/etag come
    // from the edited occurrence; a fresh event gets a new UUID. `rrule`
    // comes from the Series control (or, for a rule this builder couldn't
    // parse, the original raw string untouched).
    let series = Rc::new(series);
    let existing_meta: Option<EventMeta> = existing.as_ref().map(|occ| (occ.uid.clone(), occ.href.clone(), occ.etag.clone()));
    let existing_attendees: Vec<Attendee> = existing.as_ref().map(|occ| occ.attendees.clone()).unwrap_or_default();
    // The delete handler needs the occurrence itself (so the caller can
    // decide resource-delete vs EXDATE), but the save handler below moves
    // `existing` into its own closure - grab the delete handler's copy now.
    let existing_for_delete = existing.clone();
    let owner_email = prefill.owner_email.clone();

    let save_handler = {
        let dialog = dialog.clone();
        let calendar_dropdown = calendar_dropdown.clone();
        let calendar_ids = calendar_ids.clone();
        let error_label = error_label.clone();
        let title_row = title_row.clone();
        let attendees_field = attendees_field.clone();
        let location_row = location_row.clone();
        let description_buffer = description_buffer.clone();
        let all_day_switch = all_day_switch.clone();
        let start_date = start_date.clone();
        let end_date = end_date.clone();
        let start_hour = start_hour.clone();
        let start_minute = start_minute.clone();
        let end_hour = end_hour.clone();
        let end_minute = end_minute.clone();
        let zone_dropdown = zone_dropdown.clone();
        let video_toggle = video_toggle.clone();
        let video_url_row = video_url_row.clone();
        let selected_categories = selected_categories.clone();
        let busy_dropdown = busy_dropdown.clone();
        let sensitivity_dropdown = sensitivity_dropdown.clone();
        let reminder_dropdown = reminder_dropdown.clone();
        let attachments = attachments.clone();
        let series = series.clone();
        let existing_meta = existing_meta.clone();
        let existing_attendees = existing_attendees.clone();
        let scope_dropdown = scope_dropdown.clone();
        let graph_calendars = prefill.graph_calendars.clone();
        let on_save = on_save;
        // The parameter is annotated so the closure is created with a
        // higher-ranked `&Button` signature from the start - an unannotated
        // `|_|` in a `let`-bound closure gets an early-bound lifetime that then
        // fails the `Fn(&Button) + 'static` bound at `connect_clicked`.
        move |_button: &gtk::Button| {
            let selected = calendar_dropdown.selected() as usize;
            let Some(calendar_id) = calendar_ids.get(selected).cloned() else {
                error_label.set_label("No calendar selected.");
                error_label.set_visible(true);
                return;
            };
            attendees_field.commit_pending();
            let all_day = all_day_switch.is_active();
            let zone = if all_day { None } else { selected_zone_name(&zone_dropdown) };
            let start_local = date_time_from_form(&start_date, &start_hour, &start_minute, all_day);
            let rrule = series.rrule(all_day);
            // Graph's structured recurrence model can't hold every RRULE this
            // builder offers. Refuse the save here, with the field the user can
            // act on, rather than letting `lookout_graph` drop the recurrence
            // (the transport refuses it too - this just names the problem).
            if let Some(raw) = rrule.as_deref() {
                if graph_calendars.contains(&calendar_id) && lookout_graph::rrule_to_graph(raw, start_local.date(), zone.as_deref()).is_none() {
                    error_label.set_label("Microsoft 365 can't store this repeat pattern. Choose another pattern.");
                    error_label.set_visible(true);
                    return;
                }
            }
            // Graph attaches a file after the event is saved and caps one at
            // 3 MB (no upload session yet); refuse here rather than leaving the
            // meeting saved without its file (CAL-21).
            if graph_calendars.contains(&calendar_id) {
                if let Some(attachment) = attachments
                    .borrow()
                    .iter()
                    .find(|attachment| attachment.byte_size().is_some_and(|size| size > GRAPH_ATTACHMENT_LIMIT_BYTES))
                {
                    error_label.set_label(&format!("\"{}\" is larger than Microsoft 365's 3 MB attachment limit.", attachment.name));
                    error_label.set_visible(true);
                    return;
                }
            }
            let input = FormInput {
                title: title_row.text().to_string(),
                location: location_row.text().to_string(),
                description: description_buffer.text(&description_buffer.start_iter(), &description_buffer.end_iter(), false).to_string(),
                all_day,
                time_zone: zone,
                start_local,
                end_local: date_time_from_form(&end_date, &end_hour, &end_minute, all_day),
                attendee_tokens: attendees_field.addresses(),
                existing_attendees: existing_attendees.clone(),
                categories: selected_categories.borrow().clone(),
                sensitivity: match sensitivity_dropdown.selected() {
                    1 => EventSensitivity::Private,
                    2 => EventSensitivity::Confidential,
                    _ => EventSensitivity::Public,
                },
                show_as: show_as_from_index(busy_dropdown.selected()),
                status: existing.as_ref().map(|occ| occ.status).unwrap_or_default(),
                reminder_minutes_before: reminder_choices.get(reminder_dropdown.selected() as usize).and_then(|(_, m)| *m),
                video_enabled: video_toggle.is_active(),
                conference_url_text: video_url_row.text().to_string(),
                graph_calendar: graph_calendars.contains(&calendar_id),
                rrule,
                recurrence_id: None,
                recurrence_range: RecurrenceRange::default(),
                exdates: existing.as_ref().map(|occ| occ.exdates.clone()).unwrap_or_default(),
                owner_email: owner_email.clone(),
                existing_meta: existing_meta.clone(),
                attachments: attachments.borrow().clone(),
                resource_attendees: resource_attendees.borrow().iter().cloned().collect(),
            };
            match build_event_from_input(input) {
                Ok(mut event) => {
                    event.calendar_id = calendar_id.clone();
                    apply_edit_scope(&mut event, scope_dropdown.selected() as usize, existing.as_ref());
                    on_save(calendar_id, event);
                    dialog.close();
                }
                Err(message) => {
                    error_label.set_label(&message);
                    error_label.set_visible(true);
                }
            }
        }
    };
    save_button.connect_clicked(save_handler);

    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }

    if has_existing {
        let dialog = dialog.clone();
        let on_delete = on_delete;
        let calendar_dropdown = calendar_dropdown.clone();
        let calendar_ids = calendar_ids.clone();
        let error_label = error_label.clone();
        let existing = existing_for_delete;
        delete_button.connect_clicked(move |_| {
            let selected = calendar_dropdown.selected() as usize;
            let Some(calendar_id) = calendar_ids.get(selected).cloned() else {
                error_label.set_label("No calendar selected.");
                error_label.set_visible(true);
                return;
            };
            // Delete doesn't validate the form's times - the user may have
            // left the fields in a broken state and still want the event gone.
            if let Some(occ) = &existing {
                on_delete(calendar_id, occ.clone());
                dialog.close();
            }
        });
    }

    // Print: snapshot the live form state (title, when, series, attendees,
    // ... exactly what the user is looking at) into a printable HTML document.
    {
        let dialog = dialog.clone();
        let calendar_labels = calendar_labels.clone();
        let calendar_dropdown = calendar_dropdown.clone();
        let title_row = title_row.clone();
        let attendees_field = attendees_field.clone();
        let location_row = location_row.clone();
        let all_day_switch = all_day_switch.clone();
        let start_date = start_date.clone();
        let start_hour = start_hour.clone();
        let start_minute = start_minute.clone();
        let end_date = end_date.clone();
        let end_hour = end_hour.clone();
        let end_minute = end_minute.clone();
        let video_toggle = video_toggle.clone();
        let video_url_row = video_url_row.clone();
        let selected_categories = selected_categories.clone();
        let busy_dropdown = busy_dropdown.clone();
        let sensitivity_dropdown = sensitivity_dropdown.clone();
        let reminder_dropdown = reminder_dropdown.clone();
        let description_buffer = description_buffer.clone();
        let series = series.clone();
        print_button.connect_clicked(move |_| {
            let categories = selected_categories.borrow().clone();
            let html = printable_event_html(
                &calendar_labels,
                &calendar_dropdown,
                &title_row,
                &attendees_field,
                &location_row,
                &all_day_switch,
                &start_date,
                &start_hour,
                &start_minute,
                &end_date,
                &end_hour,
                &end_minute,
                &video_toggle,
                &video_url_row,
                &categories,
                &busy_dropdown,
                &sensitivity_dropdown,
                &reminder_dropdown,
                reminder_choices,
                &description_buffer,
                &series,
            );
            crate::window::print_html_once(&html, &dialog);
        });
    }

    // Read-only (webcal subscription) mode: lock every input and hide the
    // actions - there's no write path for feeds, so Save/Delete are not just
    // insensitive but hidden to avoid implying they could work.
    if read_only {
        for widget in [
            title_row.upcast_ref::<gtk::Widget>(),
            attendees_field.widget().upcast_ref::<gtk::Widget>(),
            location_row.upcast_ref::<gtk::Widget>(),
            all_day_switch.upcast_ref::<gtk::Widget>(),
            start_calendar.upcast_ref::<gtk::Widget>(),
            end_calendar.upcast_ref::<gtk::Widget>(),
            start_hour.upcast_ref::<gtk::Widget>(),
            start_minute.upcast_ref::<gtk::Widget>(),
            end_hour.upcast_ref::<gtk::Widget>(),
            end_minute.upcast_ref::<gtk::Widget>(),
            video_toggle.upcast_ref::<gtk::Widget>(),
            video_url_row.upcast_ref::<gtk::Widget>(),
            description_view.upcast_ref::<gtk::Widget>(),
        ] {
            widget.set_sensitive(false);
        }
        for widget in [
            series.button.upcast_ref::<gtk::Widget>(),
            busy_dropdown.upcast_ref::<gtk::Widget>(),
            categorize_button.upcast_ref::<gtk::Widget>(),
            reminder_dropdown.upcast_ref::<gtk::Widget>(),
            sensitivity_dropdown.upcast_ref::<gtk::Widget>(),
            save_button.upcast_ref::<gtk::Widget>(),
        ] {
            widget.set_sensitive(false);
        }
        delete_button.set_visible(false);
        // A read-only event can't book a room.
        if let Some(button) = &room_button {
            button.set_visible(false);
        }
    }

    dialog.present();
}

/// Builds the event editor's Attachments group (CAL-21) and returns its shared
/// list. Rows are rebuilt in place after every add/remove; an attachment that
/// already carries inline contents gets a Save button, and a writable event
/// also gets Add and Remove. A file over
/// [`MAX_INLINE_EVENT_ATTACHMENT_BYTES`] is refused with an explanation rather
/// than being silently dropped.
fn build_event_attachments_group(
    initial: Vec<EventAttachment>,
    read_only: bool,
    calendar_id: CalendarId,
    event_href: Option<String>,
    attachment_fetch: Option<AttachmentFetchCallback>,
) -> (adw::PreferencesGroup, Rc<RefCell<Vec<EventAttachment>>>) {
    let attachments = Rc::new(RefCell::new(initial));
    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let group = adw::PreferencesGroup::builder().title("Attachments").build();

    if !read_only {
        let add_row = adw::ActionRow::builder()
            .title("Add attachment")
            .subtitle(format!("Files up to {} MB", MAX_INLINE_EVENT_ATTACHMENT_BYTES / (1024 * 1024)))
            .build();
        let add_button = gtk::Button::with_label("Add");
        add_button.set_valign(gtk::Align::Center);
        add_button.set_tooltip_text(Some("Attach a file to this event"));
        add_row.add_suffix(&add_button);
        group.add(&add_row);

        let group_for_add = group.clone();
        let attachments_for_add = attachments.clone();
        let rows_for_add = rows.clone();
        let calendar_id_for_add = calendar_id.clone();
        let event_href_for_add = event_href.clone();
        let attachment_fetch_for_add = attachment_fetch.clone();
        add_button.connect_clicked(move |button| {
            let Some(window) = button.root().and_downcast::<gtk::Window>() else { return };
            let group = group_for_add.clone();
            let attachments = attachments_for_add.clone();
            let rows = rows_for_add.clone();
            let calendar_id = calendar_id_for_add.clone();
            let event_href = event_href_for_add.clone();
            let attachment_fetch = attachment_fetch_for_add.clone();
            glib::spawn_future_local(async move {
                let dialog = gtk::FileDialog::builder().title("Attach files").build();
                let Ok(files) = dialog.open_multiple_future(Some(&window)).await else { return };
                let mut too_large = Vec::new();
                for i in 0..files.n_items() {
                    let Some(file) = files.item(i).and_downcast::<gio::File>() else { continue };
                    let Ok(info) = file
                        .query_info_future("standard::display-name,standard::content-type", gio::FileQueryInfoFlags::NONE, glib::Priority::DEFAULT)
                        .await
                    else {
                        continue;
                    };
                    let display_name = info.display_name().to_string();
                    let mime_type = info
                        .content_type()
                        .and_then(|content_type| gio::functions::content_type_get_mime_type(&content_type))
                        .map(|mime| mime.to_string());
                    let Ok((bytes, _etag)) = file.load_contents_future().await else { continue };
                    match EventAttachment::from_bytes(display_name.clone(), mime_type, &bytes) {
                        Some(attachment) => attachments.borrow_mut().push(attachment),
                        None => too_large.push(display_name),
                    }
                }
                rebuild_attachment_rows(&group, &attachments, &rows, false, &calendar_id, &event_href, &attachment_fetch);
                if !too_large.is_empty() {
                    group.set_description(Some(&format!(
                        "{} couldn't be attached: the limit is {} MB per file.",
                        too_large.join(", "),
                        MAX_INLINE_EVENT_ATTACHMENT_BYTES / (1024 * 1024)
                    )));
                }
            });
        });
    }

    rebuild_attachment_rows(&group, &attachments, &rows, read_only, &calendar_id, &event_href, &attachment_fetch);
    (group, attachments)
}

/// Recreates the per-attachment rows under the group. The "Add attachment"
/// row (when writable) stays first; everything after it is rebuilt from
/// `attachments`, so indexes in the Remove handlers always match the list.
fn rebuild_attachment_rows(
    group: &adw::PreferencesGroup,
    attachments: &Rc<RefCell<Vec<EventAttachment>>>,
    rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    read_only: bool,
    calendar_id: &CalendarId,
    event_href: &Option<String>,
    attachment_fetch: &Option<AttachmentFetchCallback>,
) {
    let existing_rows = std::mem::take(&mut *rows.borrow_mut());
    for row in existing_rows {
        group.remove(&row);
    }
    let items = attachments.borrow().clone();
    for (index, attachment) in items.iter().enumerate() {
        let row = adw::ActionRow::builder().title(&attachment.name).subtitle(attachment_subtitle(attachment)).build();
        if let Some(bytes) = attachment.bytes() {
            let save = gtk::Button::from_icon_name("document-save-symbolic");
            save.add_css_class("flat");
            save.set_tooltip_text(Some("Save attachment"));
            save.set_valign(gtk::Align::Center);
            let name = attachment.name.clone();
            save.connect_clicked(move |button| {
                let Some(window) = button.root().and_downcast::<gtk::Window>() else { return };
                save_event_attachment(&window, &name, bytes.clone());
            });
            row.add_suffix(&save);
        } else if let (Some(fetch), Some(_)) = (attachment_fetch, attachment.url.as_deref()) {
            // A server-managed CalDAV attachment or a Graph child resource:
            // its bytes live on the server, so save goes through the owning
            // calendar session (CAL-21).
            let save = gtk::Button::from_icon_name("document-save-symbolic");
            save.add_css_class("flat");
            save.set_tooltip_text(Some("Save attachment"));
            save.set_valign(gtk::Align::Center);
            let fetch = fetch.clone();
            let attachment = attachment.clone();
            let calendar_id = calendar_id.clone();
            let event_href = event_href.clone();
            let error_group = group.clone();
            save.connect_clicked(move |button| {
                let Some(window) = button.root().and_downcast::<gtk::Window>() else { return };
                let window = std::rc::Rc::new(window);
                let success_window = window.clone();
                let on_success: Rc<dyn Fn(String, Vec<u8>)> = Rc::new(move |name, data| {
                    save_event_attachment(&success_window, &name, data);
                });
                let on_error: Rc<dyn Fn(String)> = {
                    let error_group = error_group.clone();
                    Rc::new(move |message| {
                        error_group.set_description(Some(&message));
                    })
                };
                fetch(calendar_id.clone(), event_href.clone(), attachment.clone(), on_success, on_error);
            });
            row.add_suffix(&save);
        }
        if !read_only {
            let remove = gtk::Button::from_icon_name("window-close-symbolic");
            remove.add_css_class("flat");
            remove.set_tooltip_text(Some("Remove attachment"));
            remove.set_valign(gtk::Align::Center);
            let group = group.clone();
            let attachments = attachments.clone();
            let rows = rows.clone();
            let calendar_id = calendar_id.clone();
            let event_href = event_href.clone();
            let attachment_fetch = attachment_fetch.clone();
            remove.connect_clicked(move |_| {
                if index < attachments.borrow().len() {
                    attachments.borrow_mut().remove(index);
                }
                rebuild_attachment_rows(&group, &attachments, &rows, false, &calendar_id, &event_href, &attachment_fetch);
            });
            row.add_suffix(&remove);
        }
        group.add(&row);
        rows.borrow_mut().push(row);
    }
    if read_only && items.is_empty() {
        group.set_visible(false);
    } else {
        group.set_visible(true);
    }
}

/// A row's subtitle: the MIME type and size, whichever are known.
fn attachment_subtitle(attachment: &EventAttachment) -> String {
    let mut parts = Vec::new();
    if let Some(mime_type) = attachment.mime_type.as_deref().filter(|mime| !mime.trim().is_empty()) {
        parts.push(mime_type.to_string());
    }
    if let Some(size) = attachment.byte_size() {
        parts.push(lookout_core::text::human_size(size));
    }
    if parts.is_empty() {
        "Attachment".to_string()
    } else {
        parts.join(" · ")
    }
}

/// Writes inline attachment bytes to a user-chosen file - the row's Save
/// button. The suggested name is reduced to its final path component: RFC 8607
/// `FILENAME` is untrusted, so it must never select a directory.
pub(crate) fn save_event_attachment(window: &gtk::Window, name: &str, bytes: Vec<u8>) {
    let file_name = std::path::Path::new(name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "attachment".to_string());
    let window = window.clone();
    glib::spawn_future_local(async move {
        let dialog = gtk::FileDialog::builder().title("Save attachment").initial_name(&file_name).build();
        let Ok(file) = dialog.save_future(Some(&window)).await else { return };
        let _ = file
            .replace_contents_bytes_future(&glib::Bytes::from(bytes.as_slice()), None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
            .await;
    });
}

/// Picks the first icon name present in the current icon theme, matching the
/// fallback convention already used elsewhere in this app
/// (`window::themed_icon_name`); duplicated here as a thin wrapper so this
/// module doesn't need a dependency on `window.rs`.
fn themed_icon(primary: &'static str, fallbacks: &'static [&'static str]) -> &'static str {
    let mut candidates = Vec::with_capacity(fallbacks.len() + 1);
    candidates.push(primary);
    candidates.extend_from_slice(fallbacks);
    crate::window::themed_icon_name(&candidates)
}

/// Formats an [`Attendee`] back into a token [`RecipientEntry::set_from_text`]
/// understands: `Name <addr>` when a display name is known, otherwise the
/// bare address.
fn attendee_to_token(attendee: &Attendee) -> String {
    match &attendee.address.name {
        Some(name) if !name.trim().is_empty() => format!("{name} <{}>", attendee.address.address),
        _ => attendee.address.address.clone(),
    }
}

/// The display-name portion of a recipient token (`Ada <ada@example.com>`
/// yields `Some("Ada")`), mirroring [`address_of`] for the other half.
fn name_of(token: &str) -> Option<String> {
    let open = token.find('<')?;
    let name = token[..open].trim().trim_matches('"').trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Response counts for a meeting's attendee list (item 3.6 / CAL-16), the
/// summary line above Outlook's Tracking list.
#[derive(Debug, Default, PartialEq, Eq)]
struct AttendeeResponses {
    accepted: usize,
    tentative: usize,
    declined: usize,
    no_response: usize,
}

fn attendee_responses(attendees: &[Attendee]) -> AttendeeResponses {
    let mut counts = AttendeeResponses::default();
    for attendee in attendees {
        match attendee.status {
            AttendeeStatus::Accepted => counts.accepted += 1,
            AttendeeStatus::Tentative => counts.tentative += 1,
            AttendeeStatus::Declined => counts.declined += 1,
            AttendeeStatus::NeedsAction => counts.no_response += 1,
        }
    }
    counts
}

fn attendee_responses_summary(counts: &AttendeeResponses) -> String {
    format!(
        "{} accepted · {} tentative · {} declined · {} no response",
        counts.accepted, counts.tentative, counts.declined, counts.no_response
    )
}

fn attendee_status_label(status: AttendeeStatus) -> &'static str {
    match status {
        AttendeeStatus::Accepted => "Accepted",
        AttendeeStatus::Tentative => "Tentative",
        AttendeeStatus::Declined => "Declined",
        AttendeeStatus::NeedsAction => "No response",
    }
}

fn attendee_status_icon(status: AttendeeStatus) -> (&'static str, &'static [&'static str]) {
    match status {
        AttendeeStatus::Accepted => ("emblem-ok-symbolic", &["object-select-symbolic"]),
        AttendeeStatus::Tentative => ("dialog-question-symbolic", &["help-about-symbolic"]),
        AttendeeStatus::Declined => ("window-close-symbolic", &["process-stop-symbolic"]),
        AttendeeStatus::NeedsAction => ("appointment-soon-symbolic", &["alarm-symbolic"]),
    }
}

/// The editor's Tracking page (item 3.6 / CAL-16): the organizer row, one row
/// per attendee carrying their stored `PARTSTAT`, and the response counts
/// above. The statuses are the occurrence snapshot the editor opened with; a
/// later server resync is reflected by reopening the editor.
fn build_tracking_page(occurrence: &EventOccurrence, owner_email: Option<&str>) -> gtk::ScrolledWindow {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let heading = gtk::Label::builder().label("Tracking").xalign(0.0).css_classes(["title-4"]).build();
    crate::privacy::mark_ui(&heading);
    content.append(&heading);
    let counts = attendee_responses(&occurrence.attendees);
    let counts_label = gtk::Label::builder()
        .label(attendee_responses_summary(&counts))
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    crate::privacy::mark_ui(&counts_label);
    content.append(&counts_label);

    let group = adw::PreferencesGroup::new();
    if let Some(organizer) = occurrence.organizer.as_ref() {
        let title = organizer.name.as_deref().map(str::trim).filter(|name| !name.is_empty()).unwrap_or(&organizer.address);
        let row = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(title))
            .subtitle(gtk::glib::markup_escape_text(&organizer.address))
            .build();
        // Organizer name/address: blurred by privacy mode.
        crate::privacy::mark_row(&row);
        let organizer_label = gtk::Label::builder().label("Organizer").css_classes(["dim-label"]).build();
        crate::privacy::mark_ui(&organizer_label);
        row.add_suffix(&organizer_label);
        group.add(&row);
    }
    for attendee in &occurrence.attendees {
        let named = attendee.address.name.as_deref().map(str::trim).is_some_and(|name| !name.is_empty());
        let title = if named {
            attendee.address.name.as_deref().unwrap_or_default().trim()
        } else {
            &attendee.address.address
        };
        let mut subtitle_parts = Vec::new();
        if named {
            subtitle_parts.push(attendee.address.address.clone());
        }
        match attendee.role {
            AttendeeRole::Optional => subtitle_parts.push("Optional".to_string()),
            // A booked room is a resource attendee (CAL-19), not a guest.
            AttendeeRole::Resource => subtitle_parts.push("Room".to_string()),
            AttendeeRole::Required => {}
        }
        if owner_email.is_some_and(|owner| owner.eq_ignore_ascii_case(&attendee.address.address)) {
            subtitle_parts.push("You".to_string());
        }
        let row = adw::ActionRow::builder().title(gtk::glib::markup_escape_text(title)).build();
        // Attendee name/address: blurred by privacy mode.
        crate::privacy::mark_row(&row);
        if !subtitle_parts.is_empty() {
            row.set_subtitle(&gtk::glib::markup_escape_text(&subtitle_parts.join(" · ")));
        }
        row.add_suffix(&tracking_status_widget(attendee.status));
        group.add(&row);
    }
    content.append(&group);

    gtk::ScrolledWindow::builder()
        .child(&content)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build()
}

/// The suffix widget for one attendee's response: a status icon plus the
/// Outlook wording (UX-60's terms, no "Maybe").
fn tracking_status_widget(status: AttendeeStatus) -> gtk::Widget {
    let (icon_name, fallbacks) = attendee_status_icon(status);
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).valign(gtk::Align::Center).build();
    row.append(&gtk::Image::from_icon_name(themed_icon(icon_name, fallbacks)));
    let status_label = gtk::Label::builder().label(attendee_status_label(status)).css_classes(["dim-label"]).build();
    crate::privacy::mark_ui(&status_label);
    row.append(&status_label);
    row.upcast()
}

/// `month_occurrences` filtered to whatever touches `day`, with the entry
/// matching `existing` (if any) excluded - the caller pushes its own live
/// `synthetic_occurrence` for `existing` instead, so leaving the stale
/// cached copy in would render the event being edited twice.
/// `recurrence_id` joins `uid` in the identity check since a recurring
/// series' instances all share one `uid`.
fn occurrences_for_day(month_occurrences: &[EventOccurrence], day: NaiveDate, existing: Option<&EventOccurrence>) -> Vec<EventOccurrence> {
    month_occurrences
        .iter()
        .filter(|o| !crate::calendar_view::covered_local_dates(o, day, day).is_empty())
        .filter(|o| existing.is_none_or(|e| o.uid != e.uid || o.recurrence_id != e.recurrence_id))
        .cloned()
        .collect()
}

/// The display zone from the form's Time zone picker (the Scheduling grid
/// draws in the event's own zone, so a click maps straight onto the form).
fn zone_from_dropdown(dropdown: &gtk::DropDown) -> DisplayZone {
    selected_zone(dropdown).map(DisplayZone::Named).unwrap_or_default()
}

/// The attendees the form currently holds, as addresses (display names kept).
fn attendee_addresses(field: &RecipientEntry) -> Vec<EmailAddress> {
    field
        .addresses()
        .iter()
        .map(|token| EmailAddress {
            name: name_of(token),
            address: address_of(token).to_string(),
        })
        .filter(|address| !address.address.is_empty())
        .collect()
}

/// The organizer's own row in the Scheduling grid: their cached events for
/// the day (excluding the event being edited), as busy intervals. Free
/// (transparent) events don't block, matching the grid's own show-as styling.
fn organizer_schedule_row(month_occurrences: &[EventOccurrence], existing: Option<&EventOccurrence>, date: NaiveDate, subtitle: Option<String>) -> ScheduleRow {
    let intervals = occurrences_for_day(month_occurrences, date, existing)
        .into_iter()
        .filter(|occ| occ.show_as != BusyStatus::Free)
        .map(|occ| FreeBusyInterval {
            start: occ.start,
            end: occ.end,
            status: occ.show_as,
        })
        .collect();
    ScheduleRow {
        label: "You".to_string(),
        subtitle,
        intervals,
        organizer: true,
        ..ScheduleRow::default()
    }
}

/// A placeholder occurrence for the event currently being created/edited,
/// rendered as an extra chip on the preview day-strip alongside whatever
/// else is cached for that day - it isn't (or is stale) in the caller's
/// snapshot, since it reflects the form's live, possibly-unsaved state.
fn synthetic_occurrence(calendar_id: CalendarId, title: &str, start: DateTime<Utc>, end: DateTime<Utc>, all_day: bool) -> EventOccurrence {
    let title = title.trim();
    EventOccurrence {
        uid: EventUid("__lookout_preview__".to_string()),
        calendar_id,
        summary: Some(if title.is_empty() { "(untitled)".to_string() } else { title.to_string() }),
        description: None,
        location: None,
        start,
        end,
        all_day,
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
        attendees: Vec::new(),
        organizer: None,
        schedule_agent: None,
        categories: Vec::new(),
        sensitivity: EventSensitivity::default(),
        show_as: BusyStatus::default(),
        status: EventStatus::default(),
        reminder_minutes_before: None,
        conference_url: None,
        attachments: Vec::new(),
    }
}

/// The reminder dropdown's starting choice: an edited event keeps its own
/// reminder; a new one (`existing` = `None`) starts at the "Default
/// reminder" preference (item 3.1). A lead time that isn't one of the
/// [`crate::settings::REMINDER_CHOICES`] shows as "No reminder".
fn initial_reminder_index(existing: Option<Option<i64>>, default: Option<i64>) -> usize {
    let wanted = existing.unwrap_or(default);
    crate::settings::REMINDER_CHOICES.iter().position(|(_, minutes)| *minutes == wanted).unwrap_or(0)
}

/// The event form's footer action label (UX-59): an appointment saves, while
/// an event with attendees sends its invitations - "Send update" when the
/// event already exists. Pure so the switch is unit-tested without GTK.
fn save_button_label(has_existing: bool, has_attendees: bool) -> &'static str {
    match (has_existing, has_attendees) {
        (_, false) if has_existing => "Save",
        (_, false) => "Add",
        (false, true) => "Send",
        (true, true) => "Send update",
    }
}

/// The default span for a fresh event: from the next whole hour to an hour
/// later, both naive times in the calendar's display zone (23:xx rolls to
/// 00:00 tomorrow).
fn default_event_times() -> (NaiveDateTime, NaiveDateTime) {
    let now = crate::calendar_tz::display_zone().now();
    let start = if now.hour() == 23 {
        (now.date() + chrono::Duration::days(1)).and_hms_opt(0, 0, 0).unwrap()
    } else {
        now.date().and_hms_opt(now.hour() + 1, 0, 0).unwrap()
    };
    (start, start + chrono::Duration::hours(1))
}

/// Combines a tracked calendar date with the hour/minute spin buttons into a
/// local naive datetime. All-day events are pinned to 00:00 - the +1-day
/// exclusive-end shift from the form's inclusive last day happens in
/// [`build_event_from_input`].
pub(crate) fn date_time_from_form(date: &Rc<Cell<NaiveDate>>, hour: &gtk::SpinButton, minute: &gtk::SpinButton, all_day: bool) -> NaiveDateTime {
    let (h, m) = if all_day { (0, 0) } else { (hour.value() as u32, minute.value() as u32) };
    date.get().and_hms_opt(h, m, 0).unwrap_or(date.get().and_hms_opt(0, 0, 0).unwrap())
}

/// A naive time in the calendar's display zone (item 3.1) -> UTC, resolving
/// DST gaps/ambiguities deterministically (see `calendar_tz::DisplayZone::
/// resolve_local`) rather than panicking.
#[cfg(test)]
fn local_to_utc(naive: NaiveDateTime) -> DateTime<Utc> {
    crate::calendar_tz::display_zone().resolve_local(naive)
}

/// One boundary of a form or dragged span -> its stored instant: an all-day
/// boundary is UTC midnight of its date (the encoding the iCalendar parser
/// produces and the writer's `VALUE=DATE` reads back), a timed one resolves
/// in the display zone. Encoding all-day days as *local* midnight stored them
/// a day early east of UTC.
fn boundary_to_utc(naive: NaiveDateTime, all_day: bool) -> DateTime<Utc> {
    crate::calendar_tz::display_zone().occurrence_from_local(naive, all_day)
}

/// Like [`boundary_to_utc`], but resolving a timed boundary in the event's own
/// zone (CAL-11) when it has one - the form's wall-clock fields are entered in
/// the "Time zone" picker's zone. Falls back to the calendar display zone when
/// the event carries none or the stored name doesn't parse.
pub(crate) fn boundary_to_utc_in(zone: Option<chrono_tz::Tz>, naive: NaiveDateTime, all_day: bool) -> DateTime<Utc> {
    zone.map(crate::calendar_tz::DisplayZone::Named)
        .unwrap_or_else(crate::calendar_tz::display_zone)
        .occurrence_from_local(naive, all_day)
}

/// A stored IANA zone name as a zone, if it parses.
fn event_zone(name: Option<&str>) -> Option<chrono_tz::Tz> {
    name.and_then(|name| name.parse().ok())
}

/// The IANA name at a time-zone dropdown index (`None` out of range).
fn zone_name_at(index: u32) -> Option<&'static str> {
    crate::calendar_tz::iana_zone_names().get(index as usize).copied()
}

/// The time-zone picker's starting entry (CAL-11): the edited event's own
/// zone when it has one, else the calendar's display zone - so a new event is
/// created in the zone the grid shows, and saving a previously zone-less event
/// pins it to the zone its wall clock was displayed in (the instants don't
/// move). An unresolvable display zone falls back to the UTC entry.
fn initial_zone_index(existing: Option<&str>) -> u32 {
    let display = crate::calendar_tz::display_zone().tz().map(|tz| tz.name().to_string());
    let wanted = existing.map(str::to_string).or(display).unwrap_or_else(|| "UTC".to_string());
    index_of_zone(&wanted)
}

/// The index of an IANA name in the picker list, or the UTC entry's (0 when
/// even UTC is missing, which can't happen with chrono-tz's own list).
fn index_of_zone(name: &str) -> u32 {
    let names = crate::calendar_tz::iana_zone_names();
    match names.iter().position(|candidate| *candidate == name) {
        Some(index) => index as u32,
        None => names.iter().position(|candidate| *candidate == "UTC").unwrap_or(0) as u32,
    }
}

/// The zone the time-zone picker currently shows.
pub(crate) fn selected_zone(dropdown: &gtk::DropDown) -> Option<chrono_tz::Tz> {
    zone_name_at(dropdown.selected()).and_then(|name| name.parse().ok())
}

/// The IANA name the picker currently shows, for storing on the event.
fn selected_zone_name(dropdown: &gtk::DropDown) -> Option<String> {
    zone_name_at(dropdown.selected()).map(str::to_string)
}

/// Builds the editor's Categorize picker (item 2.1 / CAL-07): a menu button
/// whose popover lists a check row per shared category, each with its preset
/// colour dot. Any category the event already carries that isn't in the master
/// list (a name written by another client) gets a row too, so it survives an
/// edit rather than being dropped. Returns the button and the working
/// selection (names), read back on save/print. Shared with the task editor.
pub(crate) fn build_category_picker(master: &[crate::categories::CategoryDef], selected: &[String], read_only: bool) -> (gtk::MenuButton, Rc<RefCell<Vec<String>>>) {
    let chosen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(selected.to_vec()));
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();

    // Master categories first (in list order), then any selected name not in
    // the master list, preserving the event's own values.
    let mut names: Vec<String> = master.iter().map(|c| c.name.clone()).collect();
    for name in selected {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            names.push(name.clone());
        }
    }

    if names.is_empty() {
        let empty = gtk::Label::builder()
            .label("No categories defined yet")
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build();
        crate::privacy::mark_ui(&empty);
        list.append(&empty);
    }
    for name in names {
        let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        if let Some(cat) = master.iter().find(|c| c.name.eq_ignore_ascii_case(&name)) {
            row.append(&crate::window::preset_dot_area(cat.color(), 12));
        }
        let check = gtk::CheckButton::builder().label(&name).sensitive(!read_only).build();
        // A category name is content: blurred by privacy mode.
        crate::privacy::mark_content(&check);
        check.set_active(chosen.borrow().iter().any(|n| n.eq_ignore_ascii_case(&name)));
        let chosen_for = chosen.clone();
        let name_for = name.clone();
        check.connect_toggled(move |c| {
            let mut set = chosen_for.borrow_mut();
            if c.is_active() {
                if !set.iter().any(|n| n.eq_ignore_ascii_case(&name_for)) {
                    set.push(name_for.clone());
                }
            } else {
                set.retain(|n| !n.eq_ignore_ascii_case(&name_for));
            }
        });
        row.append(&check);
        list.append(&row);
    }

    let popover = gtk::Popover::builder().child(&list).build();
    let button = gtk::MenuButton::builder()
        .icon_name(themed_icon("tag-symbolic", &["mail-mark-important-symbolic"]))
        .tooltip_text("Categorize")
        .sensitive(!read_only)
        .popover(&popover)
        .build();
    (button, chosen)
}

/// Renders the editor's current form state as a self-contained HTML document
/// for the Print action - a snapshot of exactly what the user is looking at,
/// so printing an event never depends on the dialog staying open (and works
/// for read-only webcal events, whose form fields are locked but still
/// carry the event's details).
#[allow(clippy::too_many_arguments)]
fn printable_event_html(
    calendar_labels: &[String],
    calendar_dropdown: &gtk::DropDown,
    title_row: &adw::EntryRow,
    attendees_field: &RecipientEntry,
    location_row: &adw::EntryRow,
    all_day_switch: &gtk::Switch,
    start_date: &Rc<Cell<NaiveDate>>,
    start_hour: &gtk::SpinButton,
    start_minute: &gtk::SpinButton,
    end_date: &Rc<Cell<NaiveDate>>,
    end_hour: &gtk::SpinButton,
    end_minute: &gtk::SpinButton,
    video_toggle: &adw::SwitchRow,
    video_url_row: &adw::EntryRow,
    categories: &[String],
    busy_dropdown: &gtk::DropDown,
    sensitivity_dropdown: &gtk::DropDown,
    reminder_dropdown: &gtk::DropDown,
    reminder_choices: &[(&str, Option<i64>)],
    description_buffer: &gtk::TextBuffer,
    series: &recurrence::SeriesControl,
) -> String {
    let esc = |s: &str| gtk::glib::markup_escape_text(s).to_string();
    let all_day = all_day_switch.is_active();
    let start_local = date_time_from_form(start_date, start_hour, start_minute, all_day);
    let end_local = date_time_from_form(end_date, end_hour, end_minute, all_day);
    let when = if all_day {
        format!("{} – {}", start_local.format("%A, %B %-e, %Y"), end_local.format("%A, %B %-e, %Y"))
    } else {
        format!("{} – {}", start_local.format("%A, %B %-e, %Y · %H:%M"), end_local.format("%A, %B %-e, %Y · %H:%M"))
    };
    let series_text = series.summary();
    let mut rows: Vec<(&str, String)> = Vec::new();
    rows.push(("When", when));
    rows.push(("Series", series_text));
    if let Some(label) = calendar_labels.get(calendar_dropdown.selected() as usize) {
        rows.push(("Calendar", label.clone()));
    }
    let location = location_row.text().to_string();
    if !location.is_empty() {
        rows.push(("Location", location));
    }
    let attendees = attendees_field.addresses().join(", ");
    if !attendees.is_empty() {
        rows.push(("Attendees", attendees));
    }
    if video_toggle.is_active() {
        let url = video_url_row.text().to_string();
        let label = if video_toggle.title() == "Teams meeting" { "Teams meeting" } else { "Video call" };
        rows.push((label, if url.is_empty() { "Yes".to_string() } else { url }));
    }
    let categories = categories.join(", ");
    if !categories.is_empty() {
        rows.push(("Categories", categories));
    }
    rows.push(("Show as", if busy_dropdown.selected() == 1 { "Free".to_string() } else { "Busy".to_string() }));
    rows.push((
        "Sensitivity",
        match sensitivity_dropdown.selected() {
            1 => "Private",
            2 => "Confidential",
            _ => "Public",
        }
        .to_string(),
    ));
    rows.push((
        "Reminder",
        reminder_choices
            .get(reminder_dropdown.selected() as usize)
            .map(|(label, _)| label.to_string())
            .unwrap_or_else(|| "No reminder".to_string()),
    ));
    let body = rows
        .into_iter()
        .map(|(label, value)| format!("<tr><th>{}</th><td>{}</td></tr>", esc(label), esc(&value)))
        .collect::<String>();
    let description = description_buffer.text(&description_buffer.start_iter(), &description_buffer.end_iter(), false).to_string();
    let description_html = if description.is_empty() {
        String::new()
    } else {
        format!("<div class=\"description\">{}</div>", esc(&description))
    };
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <style>body {{ font-family: sans-serif; color: #1f2328; margin: 2em; }} \
         h1 {{ font-size: 1.5em; margin: 0 0 0.8em; }} \
         table {{ border-collapse: collapse; }} \
         th {{ text-align: left; padding: 0.2em 1.5em 0.2em 0; vertical-align: top; white-space: nowrap; color: #57606a; font-weight: 600; }} \
         td {{ padding: 0.2em 0; }} \
         .description {{ margin-top: 1.2em; white-space: pre-wrap; }}</style>\
         </head><body><h1>{}</h1><table>{}</table>{}</body></html>",
        esc(&title_row.text()),
        body,
        description_html,
    )
}

/// Plain data extracted from the form's widgets, kept separate from the
/// widgets themselves so [`build_event_from_input`] is a pure function -
/// testable without a GTK display.
struct FormInput {
    title: String,
    location: String,
    description: String,
    all_day: bool,
    /// The event's own IANA time zone (CAL-11) from the picker: the
    /// wall-clock `start_local`/`end_local` resolve in it. `None` for an
    /// all-day event, whose `VALUE=DATE` boundaries are zone-independent.
    time_zone: Option<String>,
    start_local: NaiveDateTime,
    /// The form's raw end (inclusive last day for all-day; +1 day exclusive
    /// shift happens below).
    end_local: NaiveDateTime,
    /// Raw tokens from `RecipientEntry::addresses()` - `Name <addr>` or a
    /// bare address, one per attendee.
    attendee_tokens: Vec<String>,
    /// The event's previous attendees, so an address that's still present
    /// after editing keeps its accept/decline status instead of resetting to
    /// "needs action".
    existing_attendees: Vec<Attendee>,
    categories: Vec<String>,
    sensitivity: EventSensitivity,
    show_as: BusyStatus,
    /// The event's lifecycle status (see [`CalendarEvent::status`]), carried
    /// through an edit so a cancelled event isn't silently revived. The form
    /// has no status control yet.
    status: EventStatus,
    reminder_minutes_before: Option<i64>,
    /// The join-link switch is on. On a Graph calendar this means "create a
    /// Teams meeting"; elsewhere it means "use `conference_url_text`".
    video_enabled: bool,
    conference_url_text: String,
    /// Whether the picked calendar is served by the Microsoft Graph backend
    /// (CAL-20). Graph creates the Teams link itself from `isOnlineMeeting`,
    /// while every other backend writes the manual join URL instead.
    graph_calendar: bool,
    rrule: Option<String>,
    /// The instance this event is an override of (see
    /// [`CalendarEvent::recurrence_id`]) - set by the scope control, not the
    /// form fields.
    recurrence_id: Option<DateTime<Utc>>,
    recurrence_range: RecurrenceRange,
    /// The series' EXDATEs, carried so a whole-series save doesn't drop them.
    exdates: Vec<DateTime<Utc>>,
    owner_email: Option<String>,
    existing_meta: Option<EventMeta>,
    /// The event's files (CAL-21), carried from the editor's attachment list.
    attachments: Vec<EventAttachment>,
    /// Addresses chosen through the room finder (CAL-19), lower-cased. A new
    /// attendee from this set is written as a [`AttendeeRole::Resource`] (a
    /// booked room) instead of an ordinary required guest.
    resource_attendees: Vec<String>,
}

/// Builds a [`CalendarEvent`] from already-extracted form values. `Err` on an
/// impossible time range, which the caller surfaces in the dialog's error
/// line. `calendar_id` is left as a placeholder the caller fills with the
/// picked calendar.
/// Builds the event a drag-reschedule drop should persist: the occurrence's
/// master fields carried verbatim (uid, href/etag, attendees, and the rest),
/// with `start`/`end` replaced by the dragged times (`end_local` in the
/// model's exclusive convention - one day after the last day for all-day
/// events). The drag path's counterpart of [`build_event_from_input`], which
/// rebuilds the same fields from the form instead.
/// Applies the recurring-edit scope choice to the finished event, right
/// before it's handed to the save route. `occurrence` is the occurrence the
/// editor opened with (None for a brand-new event). A no-op for non-recurring
/// events (there's only one instance, so there's no scope to choose) -
/// safe to call unconditionally from every save path.
///
/// - "All events" (2): the event is the master itself. When the occurrence
///   being edited was derived from a per-occurrence override, the target
///   resource becomes the master's own (an override-derived occurrence
///   carries the master's href/etag separately), not the override's.
/// - "This occurrence" (0): the event becomes a per-occurrence override
///   VEVENT anchored at the series instance it replaces (`RECURRENCE-ID` =
///   the instance's original series time, not the form's possibly-edited
///   times). A plain expansion creates a brand-new resource (no href/etag →
///   `If-None-Match`); an existing override is updated in place, keeping its
///   own href/etag.
/// - "This and following" (1): like "This occurrence", but the override
///   carries `RANGE=THISANDFUTURE` and keeps the series rule, so it also
///   replaces every later instance.
pub(crate) fn apply_edit_scope(event: &mut CalendarEvent, scope: usize, occurrence: Option<&EventOccurrence>) {
    let recurring = occurrence.is_some_and(|occ| occ.rrule.is_some() || occ.recurrence_id.is_some());
    if !recurring {
        return;
    }
    let from_override = occurrence.is_some_and(|occ| occ.recurrence_id.is_some());
    match scope {
        // "This occurrence": a single-instance override.
        0 => {
            event.recurrence_id = occurrence.and_then(|occ| occ.recurrence_id).or_else(|| occurrence.map(|occ| occ.start));
            event.recurrence_range = RecurrenceRange::This;
            // A single-instance override is a non-recurring VEVENT, and the
            // master's EXDATEs belong to the master - neither rides along.
            event.rrule = None;
            event.exdates = Vec::new();
            if !from_override {
                event.href = None;
                event.etag = None;
            }
        }
        // "This and following": a THISANDFUTURE override.
        1 => {
            event.recurrence_id = occurrence.and_then(|occ| occ.recurrence_id).or_else(|| occurrence.map(|occ| occ.start));
            event.recurrence_range = RecurrenceRange::ThisAndFuture;
            event.exdates = Vec::new();
            if !from_override {
                event.href = None;
                event.etag = None;
            }
        }
        // "All events": the master's own resource is the update target even
        // when the occurrence came from an override.
        _ => {
            if from_override {
                event.href = occurrence.and_then(|occ| occ.master_href.clone()).or_else(|| occurrence.and_then(|occ| occ.href.clone()));
                event.etag = occurrence.and_then(|occ| occ.master_etag.clone()).or_else(|| occurrence.and_then(|occ| occ.etag.clone()));
            }
        }
    }
}

/// The span a drag-reschedule drop hands [`calendar_event_from_occurrence`]:
/// the dragged instants as display-zone naive times, with an all-day end
/// turned from the model's exclusive day-after into the inclusive last day
/// that function expects. Passing the exclusive end straight through
/// lengthened an all-day event by a day on every drag.
pub fn dragged_local_span(occ: &EventOccurrence, new_start: DateTime<Utc>, new_end: DateTime<Utc>) -> (NaiveDateTime, NaiveDateTime) {
    let (start, end) = crate::calendar_tz::occurrence_span(crate::calendar_tz::display_zone(), new_start, new_end, occ.all_day);
    let end = if occ.all_day { (end - chrono::Duration::days(1)).max(start) } else { end };
    (start, end)
}

pub fn calendar_event_from_occurrence(occ: &EventOccurrence, start_local: NaiveDateTime, end_local: NaiveDateTime) -> CalendarEvent {
    let model_end = if occ.all_day { end_local + chrono::Duration::days(1) } else { end_local };
    CalendarEvent {
        uid: occ.uid.clone(),
        calendar_id: occ.calendar_id.clone(),
        summary: occ.summary.clone(),
        description: occ.description.clone(),
        location: occ.location.clone(),
        start: boundary_to_utc(start_local, occ.all_day),
        end: boundary_to_utc(model_end, occ.all_day),
        all_day: occ.all_day,
        time_zone: occ.time_zone.clone(),
        rrule: occ.rrule.clone(),
        recurrence_id: occ.recurrence_id,
        recurrence_range: RecurrenceRange::default(),
        exdates: occ.exdates.clone(),
        rdates: Vec::new(),
        href: occ.href.clone(),
        etag: occ.etag.clone(),
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

fn build_event_from_input(input: FormInput) -> Result<CalendarEvent, String> {
    let model_end = if input.all_day { input.end_local + chrono::Duration::days(1) } else { input.end_local };
    if model_end <= input.start_local {
        return Err("The event's end must be after its start.".to_string());
    }

    let attendees: Vec<Attendee> = input
        .attendee_tokens
        .iter()
        .map(|token| {
            let address = address_of(token).to_string();
            // An attendee that survives the edit keeps both its role and its
            // response (CAL-16's tracking would otherwise be rewritten by any
            // save: an Optional guest was being promoted to Required).
            let existing = input.existing_attendees.iter().find(|a| a.address.address.eq_ignore_ascii_case(&address));
            let status = existing.map(|a| a.status).unwrap_or(AttendeeStatus::NeedsAction);
            let is_room = input.resource_attendees.iter().any(|room| room.eq_ignore_ascii_case(&address));
            let role = existing.map(|a| a.role).unwrap_or(if is_room { AttendeeRole::Resource } else { AttendeeRole::Required });
            Attendee {
                address: EmailAddress { name: name_of(token), address },
                role,
                status,
            }
        })
        .collect();
    // RFC 5545 §3.6.1 requires an ORGANIZER whenever ATTENDEEs are present;
    // only set when both hold.
    let organizer = if attendees.is_empty() { None } else { input.owner_email.map(EmailAddress::new) };

    let conference_url = if input.video_enabled && !input.conference_url_text.trim().is_empty() {
        Some(input.conference_url_text.trim().to_string())
    } else {
        None
    };
    // CAL-20: Graph creates the Teams link from `isOnlineMeeting` alone, so on
    // a Microsoft 365 calendar the switch is the flag and the URL stays None.
    // Other backends keep the RFC 7986 `CONFERENCE` URL path (and only a
    // Teams URL marks the event as a Teams meeting for a later Graph write).
    let is_online_meeting = if input.graph_calendar {
        input.video_enabled
    } else {
        lookout_core::interop::is_teams_meeting(conference_url.as_deref())
    };

    let (uid, href, etag) = input.existing_meta.unwrap_or_else(|| (EventUid(uuid::Uuid::new_v4().to_string()), None, None));

    let summary = input.title.trim();
    let location = input.location.trim();
    let description = input.description.trim();
    let zone = event_zone(input.time_zone.as_deref());
    let time_zone = if input.all_day { None } else { input.time_zone };

    Ok(CalendarEvent {
        uid,
        calendar_id: CalendarId(String::new()),
        summary: (!summary.is_empty()).then(|| summary.to_string()),
        description: (!description.is_empty()).then(|| description.to_string()),
        location: (!location.is_empty()).then(|| location.to_string()),
        start: boundary_to_utc_in(zone, input.start_local, input.all_day),
        end: boundary_to_utc_in(zone, model_end, input.all_day),
        all_day: input.all_day,
        time_zone,
        rrule: input.rrule,
        recurrence_id: input.recurrence_id,
        recurrence_range: input.recurrence_range,
        exdates: input.exdates,
        rdates: Vec::new(),
        href,
        etag,
        // The composition wrapper (`calendar_invites::save_event_with_invites`)
        // stamps both from the previous occurrence: an edit bumps SEQUENCE
        // and keeps any SCHEDULE-AGENT override another client stored.
        sequence: 0,
        attendees,
        organizer,
        schedule_agent: None,
        categories: input.categories,
        sensitivity: input.sensitivity,
        show_as: input.show_as,
        status: input.status,
        reminder_minutes_before: input.reminder_minutes_before,
        is_online_meeting,
        conference_url,
        attachments: input.attachments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> FormInput {
        let start = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap().and_hms_opt(9, 0, 0).unwrap();
        FormInput {
            title: "Sync".to_string(),
            location: String::new(),
            description: String::new(),
            all_day: false,
            time_zone: None,
            start_local: start,
            end_local: start + chrono::Duration::hours(1),
            attendee_tokens: Vec::new(),
            existing_attendees: Vec::new(),
            categories: Vec::new(),
            sensitivity: EventSensitivity::Public,
            show_as: BusyStatus::Busy,
            status: EventStatus::Confirmed,
            reminder_minutes_before: None,
            video_enabled: false,
            conference_url_text: String::new(),
            graph_calendar: false,
            rrule: None,
            recurrence_id: None,
            recurrence_range: RecurrenceRange::default(),
            exdates: Vec::new(),
            owner_email: None,
            existing_meta: None,
            attachments: Vec::new(),
            resource_attendees: Vec::new(),
        }
    }

    #[test]
    fn build_event_from_input_keeps_the_attachment_list() {
        let mut input = base_input();
        input.attachments = vec![lookout_core::EventAttachment::from_bytes("notes.txt", Some("text/plain".to_string()), b"Hello agenda!").unwrap()];
        let event = build_event_from_input(input).expect("valid range");
        assert_eq!(event.attachments.len(), 1);
        assert_eq!(event.attachments[0].name, "notes.txt");
        assert_eq!(event.attachments[0].bytes().as_deref(), Some(b"Hello agenda!".as_slice()));
    }

    /// An all-day event chosen for 13 Oct is written as `DTSTART;VALUE=DATE:
    /// 20261013` whatever the machine's zone (run with `TZ=...` to vary it):
    /// all-day spans are encoded as UTC midnight of their dates, the shape
    /// the iCalendar parser produces, never as local midnight.
    #[test]
    fn all_day_event_is_written_as_its_chosen_date() {
        let mut input = base_input();
        input.all_day = true;
        let day = NaiveDate::from_ymd_opt(2026, 10, 13).unwrap();
        input.start_local = day.and_hms_opt(0, 0, 0).unwrap();
        input.end_local = day.and_hms_opt(0, 0, 0).unwrap();
        let event = build_event_from_input(input).expect("valid all-day range");
        assert_eq!(event.start, day.and_hms_opt(0, 0, 0).unwrap().and_utc());
        assert_eq!(event.end, (day + chrono::Duration::days(1)).and_hms_opt(0, 0, 0).unwrap().and_utc());
        let ics = lookout_dav::build_vcalendar(&event);
        assert!(ics.contains("DTSTART;VALUE=DATE:20261013"), "{ics}");
        assert!(ics.contains("DTEND;VALUE=DATE:20261014"), "{ics}");
    }

    /// CAL-11: a timed event built with a time zone resolves its wall-clock
    /// times in that zone and stores the IANA name; an all-day event drops it
    /// (its `VALUE=DATE` boundaries are zone-independent).
    #[test]
    fn time_zone_resolves_the_forms_wall_clock_times() {
        let mut input = base_input();
        input.time_zone = Some("Europe/Berlin".to_string());
        // base_input is 12 Aug 2026 09:00-10:00 wall clock; CEST is UTC+2.
        let event = build_event_from_input(input).expect("valid range");
        assert_eq!(event.time_zone.as_deref(), Some("Europe/Berlin"));
        assert_eq!(event.start, "2026-08-12T07:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(event.end, "2026-08-12T08:00:00Z".parse::<DateTime<Utc>>().unwrap());
        let ics = lookout_dav::build_vcalendar(&event);
        assert!(ics.contains("DTSTART;TZID=Europe/Berlin:20260812T090000"), "{ics}");
        assert!(ics.contains("BEGIN:VTIMEZONE"), "{ics}");

        let mut all_day = base_input();
        all_day.all_day = true;
        all_day.time_zone = Some("Europe/Berlin".to_string());
        let event = build_event_from_input(all_day).expect("valid all-day range");
        assert_eq!(event.time_zone, None, "an all-day event has no zone");
    }

    /// CAL-11: the picker starts on the edited event's own zone, and an
    /// unknown stored name falls back to the UTC entry.
    #[test]
    fn zone_picker_starts_on_the_events_zone() {
        assert_eq!(zone_name_at(initial_zone_index(Some("Asia/Kolkata"))), Some("Asia/Kolkata"));
        assert_eq!(zone_name_at(initial_zone_index(Some("Mars/Olympus_Mons"))), Some("UTC"));
        assert_eq!(zone_name_at(index_of_zone("Europe/Berlin")), Some("Europe/Berlin"));
    }

    fn occ(uid: &str, day: NaiveDate, recurrence_id: Option<NaiveDateTime>) -> EventOccurrence {
        let start = day.and_hms_opt(9, 0, 0).unwrap();
        EventOccurrence {
            uid: EventUid(uid.to_string()),
            calendar_id: CalendarId("test".to_string()),
            summary: Some(uid.to_string()),
            description: None,
            location: None,
            start: local_to_utc(start),
            end: local_to_utc(start + chrono::Duration::hours(1)),
            all_day: false,
            time_zone: None,
            rrule: None,
            recurrence_id: recurrence_id.map(local_to_utc),
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
            sequence: 0,
            attendees: Vec::new(),
            organizer: None,
            schedule_agent: None,
            categories: Vec::new(),
            sensitivity: EventSensitivity::default(),
            show_as: BusyStatus::default(),
            status: EventStatus::default(),
            reminder_minutes_before: None,
            conference_url: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn occurrences_for_day_excludes_the_event_being_edited() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let editing = occ("evt-1", day, None);
        let other = occ("evt-2", day, None);
        let result = occurrences_for_day(&[editing.clone(), other.clone()], day, Some(&editing));
        let uids: Vec<&str> = result.iter().map(|o| o.uid.0.as_str()).collect();
        assert_eq!(
            uids,
            vec!["evt-2"],
            "the cached copy of the event being edited must not appear - the caller adds its own live preview chip instead"
        );
    }

    #[test]
    fn occurrences_for_day_returns_everything_for_a_brand_new_event() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let a = occ("evt-1", day, None);
        let b = occ("evt-2", day, None);
        let result = occurrences_for_day(&[a.clone(), b.clone()], day, None);
        let uids: Vec<&str> = result.iter().map(|o| o.uid.0.as_str()).collect();
        assert_eq!(uids, vec!["evt-1", "evt-2"], "with nothing being edited yet, nothing should be excluded");
    }

    #[test]
    fn occurrences_for_day_keeps_a_different_instance_of_the_same_recurring_series() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let recurrence_a = day.and_hms_opt(9, 0, 0).unwrap();
        let recurrence_b = day.and_hms_opt(15, 0, 0).unwrap();
        let editing = occ("series", day, Some(recurrence_a));
        let sibling = occ("series", day, Some(recurrence_b));
        let result = occurrences_for_day(&[editing.clone(), sibling.clone()], day, Some(&editing));
        assert_eq!(
            result.iter().map(|o| o.recurrence_id).collect::<Vec<_>>(),
            vec![sibling.recurrence_id],
            "a sibling instance sharing the series' uid but a different recurrence_id is a distinct occurrence and must survive"
        );
    }

    #[test]
    fn rejects_end_before_start() {
        let mut input = base_input();
        input.end_local = input.start_local - chrono::Duration::hours(1);
        assert!(build_event_from_input(input).is_err());
    }

    #[test]
    fn includes_new_attendees_as_needs_action() {
        let mut input = base_input();
        input.attendee_tokens = vec!["Alice <alice@example.com>".to_string()];
        input.owner_email = Some("me@example.com".to_string());
        let event = build_event_from_input(input).unwrap();
        assert_eq!(event.attendees.len(), 1);
        assert_eq!(event.attendees[0].address.address, "alice@example.com");
        assert_eq!(event.attendees[0].address.name.as_deref(), Some("Alice"));
        assert_eq!(event.attendees[0].status, AttendeeStatus::NeedsAction);
        assert_eq!(event.organizer.as_ref().map(|o| o.address.as_str()), Some("me@example.com"));
    }

    #[test]
    fn preserves_existing_attendee_status() {
        let mut input = base_input();
        input.attendee_tokens = vec!["alice@example.com".to_string()];
        input.existing_attendees = vec![Attendee {
            address: EmailAddress::new("alice@example.com"),
            role: AttendeeRole::Required,
            status: AttendeeStatus::Accepted,
        }];
        let event = build_event_from_input(input).unwrap();
        assert_eq!(event.attendees[0].status, AttendeeStatus::Accepted);
    }

    #[test]
    fn preserves_existing_attendee_role() {
        // CAL-16: tracking shows who's Required and who's Optional, so a save
        // must not silently promote an optional guest.
        let mut input = base_input();
        input.attendee_tokens = vec!["bob@example.com".to_string(), "carol@example.com".to_string()];
        input.existing_attendees = vec![Attendee {
            address: EmailAddress::new("bob@example.com"),
            role: AttendeeRole::Optional,
            status: AttendeeStatus::Tentative,
        }];
        let event = build_event_from_input(input).unwrap();
        assert_eq!(event.attendees[0].role, AttendeeRole::Optional);
        assert_eq!(event.attendees[1].role, AttendeeRole::Required, "a new attendee is Required by default");
    }

    #[test]
    fn attendee_responses_count_and_summarise_every_status() {
        let attendee = |status| Attendee {
            address: EmailAddress::new("someone@example.com".to_string()),
            role: AttendeeRole::Required,
            status,
        };
        let attendees = vec![
            attendee(AttendeeStatus::Accepted),
            attendee(AttendeeStatus::Accepted),
            attendee(AttendeeStatus::Tentative),
            attendee(AttendeeStatus::Declined),
            attendee(AttendeeStatus::NeedsAction),
            attendee(AttendeeStatus::NeedsAction),
        ];
        let counts = attendee_responses(&attendees);
        assert_eq!(
            counts,
            AttendeeResponses {
                accepted: 2,
                tentative: 1,
                declined: 1,
                no_response: 2
            }
        );
        assert_eq!(attendee_responses_summary(&counts), "2 accepted · 1 tentative · 1 declined · 2 no response");
        assert_eq!(attendee_responses(&[]), AttendeeResponses::default(), "an empty list summarises to zeroes");
    }

    #[test]
    fn attendee_status_labels_use_outlook_wording() {
        assert_eq!(attendee_status_label(AttendeeStatus::NeedsAction), "No response");
        assert_eq!(attendee_status_label(AttendeeStatus::Tentative), "Tentative", "never the old \"Maybe\"");
        assert_eq!(attendee_status_label(AttendeeStatus::Accepted), "Accepted");
        assert_eq!(attendee_status_label(AttendeeStatus::Declined), "Declined");
    }

    #[test]
    fn omits_organizer_without_attendees_even_with_owner_email() {
        let mut input = base_input();
        input.owner_email = Some("me@example.com".to_string());
        let event = build_event_from_input(input).unwrap();
        assert!(event.organizer.is_none());
    }

    /// UX-59: the footer verb tracks whether the event has attendees.
    #[test]
    fn footer_action_label_follows_the_attendee_list() {
        assert_eq!(save_button_label(false, false), "Add", "a new appointment adds");
        assert_eq!(save_button_label(true, false), "Save", "an existing appointment saves");
        assert_eq!(save_button_label(false, true), "Send", "a new meeting sends its invitation");
        assert_eq!(save_button_label(true, true), "Send update", "an edited meeting sends an update");
    }

    #[test]
    fn omits_conference_url_when_toggle_off_even_if_url_field_has_text() {
        let mut input = base_input();
        input.video_enabled = false;
        input.conference_url_text = "https://example.com/join".to_string();
        let event = build_event_from_input(input).unwrap();
        assert!(event.conference_url.is_none());
    }

    #[test]
    fn includes_conference_url_when_toggle_on() {
        let mut input = base_input();
        input.video_enabled = true;
        input.conference_url_text = "https://example.com/join".to_string();
        let event = build_event_from_input(input).unwrap();
        assert_eq!(event.conference_url.as_deref(), Some("https://example.com/join"));
    }

    /// CAL-20: on a Microsoft 365 calendar the toggle alone asks Graph to
    /// create the Teams link, so no join URL is typed or written.
    #[test]
    fn graph_calendar_toggle_asks_graph_to_create_a_teams_link() {
        let mut input = base_input();
        input.graph_calendar = true;
        input.video_enabled = true;
        let event = build_event_from_input(input).unwrap();
        assert!(event.is_online_meeting);
        assert!(event.conference_url.is_none(), "Graph fills in the join URL");
    }

    /// Turning the Teams switch off must clear the link, not just ignore the
    /// last URL left in the hidden field.
    #[test]
    fn graph_calendar_switch_off_clears_the_existing_link() {
        let mut input = base_input();
        input.graph_calendar = true;
        input.video_enabled = false;
        input.conference_url_text = "https://teams.microsoft.com/l/meetup-join/abc".to_string();
        let event = build_event_from_input(input).unwrap();
        assert!(!event.is_online_meeting);
        assert!(event.conference_url.is_none());
    }

    /// An existing Teams link carried into an edit is preserved while the
    /// switch stays on.
    #[test]
    fn graph_calendar_keeps_an_existing_join_url() {
        let mut input = base_input();
        input.graph_calendar = true;
        input.video_enabled = true;
        input.conference_url_text = "https://teams.microsoft.com/l/meetup-join/abc".to_string();
        let event = build_event_from_input(input).unwrap();
        assert!(event.is_online_meeting);
        assert_eq!(event.conference_url.as_deref(), Some("https://teams.microsoft.com/l/meetup-join/abc"));
    }

    /// A non-Graph calendar still writes the RFC 7986 `CONFERENCE` URL, and
    /// only a Teams URL additionally marks the event as a Teams meeting.
    #[test]
    fn non_graph_calendar_still_classifies_the_join_url() {
        let mut input = base_input();
        input.video_enabled = true;
        input.conference_url_text = "https://teams.microsoft.com/l/meetup-join/abc".to_string();
        let event = build_event_from_input(input).unwrap();
        assert!(event.is_online_meeting);
        assert_eq!(event.conference_url.as_deref(), Some("https://teams.microsoft.com/l/meetup-join/abc"));
    }

    #[test]
    fn calendar_event_from_occurrence_preserves_metadata_and_replaces_times() {
        let start = NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, 12).unwrap(), chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        let occ = EventOccurrence {
            uid: EventUid("evt-1@example.com".to_string()),
            calendar_id: CalendarId("work".to_string()),
            summary: Some("Sync".to_string()),
            description: Some("Agenda".to_string()),
            location: Some("HQ".to_string()),
            start: local_to_utc(start),
            end: local_to_utc(start + chrono::Duration::hours(1)),
            all_day: false,
            time_zone: None,
            rrule: None,
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: Some("https://dav.example.com/cal/evt-1.ics".to_string()),
            etag: Some("\"abc\"".to_string()),
            master_href: None,
            master_etag: None,
            sequence: 0,
            attendees: vec![Attendee {
                address: EmailAddress::new("alice@example.com"),
                role: AttendeeRole::Required,
                status: AttendeeStatus::Accepted,
            }],
            organizer: Some(EmailAddress::new("me@example.com")),
            schedule_agent: None,
            categories: vec!["team".to_string()],
            sensitivity: EventSensitivity::Private,
            show_as: BusyStatus::Free,
            status: EventStatus::default(),
            reminder_minutes_before: Some(10),
            conference_url: Some("https://meet.example.com/x".to_string()),
            attachments: Vec::new(),
        };
        let moved = NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, 13).unwrap(), chrono::NaiveTime::from_hms_opt(14, 0, 0).unwrap());
        let event = calendar_event_from_occurrence(&occ, moved, moved + chrono::Duration::hours(2));
        assert_eq!(event.uid, occ.uid);
        assert_eq!(event.calendar_id, occ.calendar_id);
        assert_eq!(event.summary.as_deref(), Some("Sync"));
        assert_eq!(event.description.as_deref(), Some("Agenda"));
        assert_eq!(event.location.as_deref(), Some("HQ"));
        assert_eq!(event.href, occ.href);
        assert_eq!(event.etag, occ.etag);
        assert_eq!(event.attendees.len(), 1);
        assert_eq!(event.attendees[0].status, AttendeeStatus::Accepted);
        assert_eq!(event.organizer, occ.organizer);
        assert_eq!(event.categories, occ.categories);
        assert_eq!(event.sensitivity, EventSensitivity::Private);
        assert_eq!(event.show_as, BusyStatus::Free);
        assert_eq!(event.reminder_minutes_before, Some(10));
        assert_eq!(event.conference_url.as_deref(), Some("https://meet.example.com/x"));
        assert_eq!(event.start, local_to_utc(moved));
        assert_eq!(event.end, local_to_utc(moved + chrono::Duration::hours(2)));
    }

    /// A new event starts at the "Default reminder"; an edited one keeps its
    /// own, even "No reminder".
    #[test]
    fn new_events_take_the_default_reminder() {
        let index_of = |minutes: Option<i64>| crate::settings::REMINDER_CHOICES.iter().position(|(_, m)| *m == minutes).unwrap();
        assert_eq!(initial_reminder_index(None, Some(15)), index_of(Some(15)));
        assert_eq!(initial_reminder_index(None, None), index_of(None));
        assert_eq!(initial_reminder_index(Some(None), Some(15)), index_of(None), "an edit keeps no reminder");
        assert_eq!(initial_reminder_index(Some(Some(60)), Some(15)), index_of(Some(60)));
        assert_eq!(initial_reminder_index(Some(Some(7)), None), 0, "an unoffered lead time shows as none");
    }

    /// CAL-05: the "Show as" dropdown offers Outlook's five states in its
    /// order, and every state round-trips through the index mapping.
    #[test]
    fn show_as_choices_cover_outlooks_order() {
        let labels: Vec<&str> = SHOW_AS_CHOICES.iter().map(|(_, label)| *label).collect();
        assert_eq!(labels, vec!["Free", "Working elsewhere", "Tentative", "Busy", "Away"]);
        for (index, (state, _)) in SHOW_AS_CHOICES.iter().enumerate() {
            assert_eq!(show_as_index(*state), index as u32);
            assert_eq!(show_as_from_index(index as u32), *state);
        }
        assert_eq!(show_as_from_index(99), BusyStatus::Busy, "an out-of-range index falls back to Busy");
    }

    /// Dragging a one-day all-day event two days later keeps it one day long
    /// (it used to grow by a day per drag), and a timed drag keeps its times.
    #[test]
    fn dragging_an_all_day_event_keeps_its_length() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap().and_hms_opt(0, 0, 0).unwrap();
        let mut occ = occ("evt", day.date(), None);
        occ.all_day = true;
        occ.start = day.and_utc();
        occ.end = (day + chrono::Duration::days(1)).and_utc();
        let shift = chrono::Duration::days(2);
        let (start, end) = dragged_local_span(&occ, occ.start + shift, occ.end + shift);
        let event = calendar_event_from_occurrence(&occ, start, end);
        assert_eq!(event.start, (day + shift).and_utc());
        assert_eq!(event.end, (day + shift + chrono::Duration::days(1)).and_utc());

        let timed = super::tests::occ("timed", day.date(), None);
        let moved = timed.start + chrono::Duration::hours(3);
        let (start, end) = dragged_local_span(&timed, moved, timed.end + chrono::Duration::hours(3));
        let event = calendar_event_from_occurrence(&timed, start, end);
        assert_eq!((event.start, event.end), (moved, timed.end + chrono::Duration::hours(3)));
    }

    #[test]
    fn calendar_event_from_occurrence_applies_the_exclusive_end_convention_for_all_day() {
        let start = NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, 12).unwrap(), chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        let occ = EventOccurrence {
            uid: EventUid("evt-2@example.com".to_string()),
            calendar_id: CalendarId("work".to_string()),
            summary: None,
            description: None,
            location: None,
            start: start.and_utc(),
            end: (start + chrono::Duration::days(2)).and_utc(),
            all_day: true,
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
            attendees: Vec::new(),
            organizer: None,
            schedule_agent: None,
            categories: Vec::new(),
            sensitivity: EventSensitivity::default(),
            show_as: BusyStatus::default(),
            status: EventStatus::default(),
            reminder_minutes_before: None,
            conference_url: None,
            attachments: Vec::new(),
        };
        let moved = NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(), chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        // The drag reports the last *day* (inclusive); the model stores the
        // day after.
        let event = calendar_event_from_occurrence(&occ, moved, moved + chrono::Duration::days(1));
        assert_eq!(event.start, moved.and_utc());
        assert_eq!(event.end, (moved + chrono::Duration::days(2)).and_utc());
        assert!(event.all_day);
    }

    #[test]
    fn apply_edit_scope_this_turns_a_plain_expansion_into_a_fresh_override() {
        let instance = local_to_utc(NaiveDateTime::new(
            NaiveDate::from_ymd_opt(2026, 8, 12).unwrap(),
            chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        ));
        let mut occ = EventOccurrence {
            uid: EventUid("evt-3@example.com".to_string()),
            calendar_id: CalendarId("work".to_string()),
            summary: Some("Sync".to_string()),
            description: None,
            location: None,
            start: instance,
            end: instance + chrono::Duration::hours(1),
            all_day: false,
            time_zone: None,
            rrule: Some("FREQ=WEEKLY".to_string()),
            recurrence_id: None,
            exdates: vec!["2026-08-19T09:00:00Z".parse().unwrap()],
            master_start: Some(instance),
            master_end: Some(instance + chrono::Duration::hours(1)),
            href: Some("https://dav.example.com/cal/master.ics".to_string()),
            etag: Some("\"m1\"".to_string()),
            master_href: None,
            master_etag: None,
            sequence: 0,
            attendees: Vec::new(),
            organizer: None,
            schedule_agent: None,
            categories: Vec::new(),
            sensitivity: EventSensitivity::default(),
            show_as: BusyStatus::default(),
            status: EventStatus::default(),
            reminder_minutes_before: None,
            conference_url: None,
            attachments: Vec::new(),
        };
        // The form's times are the *edited* times - the override anchors the
        // original series instance.
        let edited = instance + chrono::Duration::hours(2);
        let mut event = calendar_event_from_occurrence(
            &occ,
            edited.with_timezone(&chrono::Local).naive_local(),
            (edited + chrono::Duration::hours(2)).with_timezone(&chrono::Local).naive_local(),
        );
        apply_edit_scope(&mut event, 0, Some(&occ));
        assert_eq!(event.recurrence_id, Some(instance), "anchors the original instance, not the edited time");
        assert_eq!(event.recurrence_range, RecurrenceRange::This);
        assert_eq!(event.rrule, None, "a single-instance override carries no RRULE");
        assert_eq!(event.exdates, Vec::<DateTime<Utc>>::new(), "the master's EXDATEs stay with the master");
        assert_eq!(event.href, None, "a fresh override is a brand-new resource");
        assert_eq!(event.etag, None);

        // Editing an *existing* override updates its own resource instead.
        occ.recurrence_id = Some(instance);
        occ.href = Some("https://dav.example.com/cal/override.ics".to_string());
        occ.etag = Some("\"o1\"".to_string());
        occ.master_href = Some("https://dav.example.com/cal/master.ics".to_string());
        occ.master_etag = Some("\"m1\"".to_string());
        let mut event = calendar_event_from_occurrence(
            &occ,
            edited.with_timezone(&chrono::Local).naive_local(),
            (edited + chrono::Duration::hours(2)).with_timezone(&chrono::Local).naive_local(),
        );
        apply_edit_scope(&mut event, 0, Some(&occ));
        assert_eq!(event.recurrence_id, Some(instance));
        assert_eq!(event.href.as_deref(), Some("https://dav.example.com/cal/override.ics"));
        assert_eq!(event.etag.as_deref(), Some("\"o1\""));

        // "This and following" keeps the series rule and marks the range.
        let mut event = calendar_event_from_occurrence(
            &occ,
            edited.with_timezone(&chrono::Local).naive_local(),
            (edited + chrono::Duration::hours(2)).with_timezone(&chrono::Local).naive_local(),
        );
        apply_edit_scope(&mut event, 1, Some(&occ));
        assert_eq!(event.recurrence_range, RecurrenceRange::ThisAndFuture);
        assert_eq!(event.rrule.as_deref(), Some("FREQ=WEEKLY"));

        // "All events" from an override targets the master's resource.
        let mut event = calendar_event_from_occurrence(
            &occ,
            edited.with_timezone(&chrono::Local).naive_local(),
            (edited + chrono::Duration::hours(2)).with_timezone(&chrono::Local).naive_local(),
        );
        event.rrule = Some("FREQ=WEEKLY".to_string());
        apply_edit_scope(&mut event, 2, Some(&occ));
        assert_eq!(event.href.as_deref(), Some("https://dav.example.com/cal/master.ics"));
        assert_eq!(event.etag.as_deref(), Some("\"m1\""));

        // Non-recurring events are untouched by any scope.
        occ.rrule = None;
        occ.recurrence_id = None;
        occ.exdates = Vec::new();
        let mut event = calendar_event_from_occurrence(
            &occ,
            edited.with_timezone(&chrono::Local).naive_local(),
            (edited + chrono::Duration::hours(2)).with_timezone(&chrono::Local).naive_local(),
        );
        apply_edit_scope(&mut event, 0, Some(&occ));
        assert_eq!(event.recurrence_id, None);
        assert_eq!(event.rrule, None);
    }

    #[test]
    fn a_room_chosen_by_the_finder_is_written_as_a_resource() {
        let mut input = base_input();
        input.attendee_tokens = vec!["Room 4 <room4@example.com>".to_string(), "grace@example.com".to_string()];
        input.resource_attendees = vec!["room4@example.com".to_string()];
        let event = build_event_from_input(input).expect("valid range");
        assert_eq!(event.attendees[0].role, AttendeeRole::Resource, "a booked room is a resource, not a guest");
        assert_eq!(event.attendees[0].address.name.as_deref(), Some("Room 4"));
        assert_eq!(event.attendees[1].role, AttendeeRole::Required, "a person from the same form stays required");
    }
}
