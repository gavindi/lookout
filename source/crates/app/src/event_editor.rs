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
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike, Utc};
use lookout_core::{
    Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, CalendarId, EmailAddress, EventOccurrence, EventSensitivity, EventTransparency, EventUid, RecurrenceRange,
};

use crate::calendar_colors::CalendarColorMap;
use crate::recipient_entry::{address_of, RecipientEntry, SuggestionSource};
use crate::recurrence;

/// The save callback's target: the picked calendar plus the finished event.
pub type SaveCallback = Rc<dyn Fn(CalendarId, CalendarEvent)>;
/// The delete callback's target: the calendar plus the occurrence being
/// deleted - the whole occurrence, so the caller can decide whether to
/// delete the resource outright (an override) or EXDATE the instance out of
/// its master (a plain expansion of a recurring series).
type DeleteCallback = Rc<dyn Fn(CalendarId, EventOccurrence)>;
/// The edited occurrence's identity + write target, threaded through the form.
type EventMeta = (EventUid, Option<String>, Option<String>);

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
}

/// Opens the event editor as a modal dialog. `on_save` fires with the chosen
/// calendar and the finished event (new or edited); `on_delete` fires with the
/// delete target. Both are responsible for routing to the owning account's
/// session - the dialog only builds and validates the event.
pub fn show_event_editor(
    window: &adw::ApplicationWindow,
    prefill: EventEditorPrefill,
    attendee_suggestions: SuggestionSource,
    on_save: impl Fn(CalendarId, CalendarEvent) + 'static,
    on_delete: impl Fn(CalendarId, EventOccurrence) + 'static,
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
    let calendar_row = adw::ActionRow::builder().title("Calendar").build();
    calendar_row.add_suffix(&calendar_dropdown);

    // --- Title / attendees / all-day toggle.
    let title_row = adw::EntryRow::builder().title("Title").build();
    let attendees_field = RecipientEntry::new("Invite required attendees");
    attendees_field.set_suggestion_source(attendee_suggestions);
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

    fn set_calendar_date(cal: &gtk::Calendar, tracked: &Rc<Cell<NaiveDate>>, date: NaiveDate) {
        tracked.set(date);
        if let Ok(dt) = gtk::glib::DateTime::from_utc(date.year(), date.month() as i32, date.day() as i32, 0, 0, 0.0) {
            cal.set_date(Some(&dt));
        }
    }
    fn wire_calendar(cal: &gtk::Calendar, tracked: &Rc<Cell<NaiveDate>>) {
        let tracked = tracked.clone();
        cal.connect_day_selected(move |cal| {
            if let Some(date) = NaiveDate::from_ymd_opt(cal.year(), cal.month() as u32 + 1, cal.day() as u32) {
                tracked.set(date);
            }
        });
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
    let (initial_start, initial_end) = existing
        .as_ref()
        .map(|occ| {
            if occ.recurrence_id.is_none() {
                if let (Some(master_start), Some(master_end)) = (occ.master_start, occ.master_end) {
                    return (
                        master_start.with_timezone(&chrono::Local).naive_local(),
                        master_end.with_timezone(&chrono::Local).naive_local(),
                    );
                }
            }
            (occ.start.with_timezone(&chrono::Local).naive_local(), occ.end.with_timezone(&chrono::Local).naive_local())
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
    {
        let start_date = start_date.clone();
        let end_date = end_date.clone();
        let start_hour = start_hour.clone();
        let start_minute = start_minute.clone();
        let end_hour = end_hour.clone();
        let end_minute = end_minute.clone();
        let end_calendar = end_calendar.clone();
        all_day_switch.connect_active_notify(move |switch| {
            let on = switch.is_active();
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
    datetime_row.append(&gtk::Label::builder().label("to").css_classes(["dim-label"]).build());
    datetime_row.append(&time_cluster("Ends", &end_calendar, &end_hour, &end_minute));
    let all_day_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .hexpand(true)
        .valign(gtk::Align::Start)
        .build();
    all_day_box.append(&gtk::Label::new(Some("All day")));
    all_day_box.append(&all_day_switch);
    datetime_row.append(&all_day_box);

    // --- Location + video call + notes.
    let location_row = adw::EntryRow::builder().title("Location").build();
    let video_toggle = adw::SwitchRow::builder().title("Video call link").subtitle("Add a join-this-meeting URL").build();
    let video_url_row = adw::EntryRow::builder().title("Meeting URL").build();
    video_url_row.set_visible(video_toggle.is_active());
    {
        let video_url_row = video_url_row.clone();
        video_toggle.connect_active_notify(move |row| video_url_row.set_visible(row.is_active()));
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

    let recurring_note = gtk::Label::builder()
        .label("This event repeats - changes apply to the whole series.")
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    recurring_note.set_visible(existing.as_ref().is_some_and(|occ| occ.rrule.is_some()));
    let read_only_note = gtk::Label::builder()
        .label(prefill.read_only_note)
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    read_only_note.set_visible(read_only);
    let error_label = gtk::Label::builder().wrap(true).xalign(0.0).halign(gtk::Align::Start).css_classes(["error"]).build();
    error_label.set_visible(false);

    let notes_label = gtk::Label::builder()
        .label("Notes")
        .css_classes(["dim-label", "caption"])
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
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
    let preview_mini = crate::calendar_view::build_mini();
    let preview_day_strip = Rc::new(crate::calendar_view::build_time_grid(&[chrono::Weekday::Mon], true));
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

    // --- Top action row: Cancel/Save plus the toolbar-style controls the
    // reference layout groups above the fields (Series, Busy, Categorize,
    // Reminder, Sensitivity), with Print/Options as disabled placeholders -
    // the same `set_sensitive(false)` convention the main calendar toolbar
    // already uses for controls with no backend behind them yet.
    let cancel_button = gtk::Button::with_label("Cancel");
    let save_button = gtk::Button::with_label(if has_existing { "Save" } else { "Add" });
    save_button.add_css_class("suggested-action");

    let series = recurrence::build_series_control(existing.as_ref().and_then(|occ| occ.rrule.as_deref()));
    series.button.set_tooltip_text(Some("Series"));

    let busy_model = gtk::StringList::new(&["Busy", "Free"]);
    let busy_dropdown = gtk::DropDown::builder().model(&busy_model).tooltip_text("Show as").build();
    busy_dropdown.set_selected(existing.as_ref().map(|occ| occ.transparency).unwrap_or_default() as u32);

    let categorize_entry = gtk::Entry::builder().placeholder_text("Work, Personal, ...").build();
    if let Some(occ) = &existing {
        if !occ.categories.is_empty() {
            categorize_entry.set_text(&occ.categories.join(", "));
        }
    }
    let categorize_popover = gtk::Popover::builder().child(&categorize_entry).build();
    let categorize_button = gtk::MenuButton::builder()
        .icon_name(themed_icon("tag-symbolic", &["mail-mark-important-symbolic"]))
        .tooltip_text("Categorize")
        .popover(&categorize_popover)
        .build();

    let reminder_choices: &[(&str, Option<i64>)] = &[
        ("No reminder", None),
        ("At time of event", Some(0)),
        ("5 minutes before", Some(5)),
        ("10 minutes before", Some(10)),
        ("15 minutes before", Some(15)),
        ("30 minutes before", Some(30)),
        ("1 hour before", Some(60)),
        ("1 day before", Some(1440)),
    ];
    let reminder_model = gtk::StringList::new(&reminder_choices.iter().map(|(label, _)| *label).collect::<Vec<_>>());
    let reminder_dropdown = gtk::DropDown::builder().model(&reminder_model).tooltip_text("Reminder").build();
    let reminder_index = existing
        .as_ref()
        .and_then(|occ| reminder_choices.iter().position(|(_, minutes)| *minutes == occ.reminder_minutes_before))
        .unwrap_or(0);
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

    let delete_button = gtk::Button::from_icon_name(themed_icon("user-trash-symbolic", &["edit-delete-symbolic"]));
    delete_button.add_css_class("destructive-action");
    delete_button.set_tooltip_text(Some("Delete"));
    delete_button.set_visible(has_existing);

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
    top_bar.append(&options_button);
    top_bar.append(&print_button);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    top_bar.append(&spacer);
    top_bar.append(&delete_button);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&top_bar);
    toolbar_view.set_content(Some(&main_paned));
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
        video_url_row.set_visible(occ.conference_url.is_some());
    }

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
        Rc::new(move || {
            let day = displayed_day.get();
            let all_day = all_day_switch.is_active();
            let start_local = date_time_from_form(&start_date, &start_hour, &start_minute, all_day);
            let end_local_raw = date_time_from_form(&end_date, &end_hour, &end_minute, all_day);
            let model_end = if all_day { end_local_raw + chrono::Duration::days(1) } else { end_local_raw };
            let selected = calendar_dropdown.selected() as usize;
            let calendar_id = calendar_ids.get(selected).cloned().unwrap_or_else(|| CalendarId(String::new()));
            let synthetic = synthetic_occurrence(calendar_id, &title_row.text(), local_to_utc(start_local), local_to_utc(model_end), all_day);
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
        let displayed_day = displayed_day.clone();
        crate::calendar_view::connect_day_selected(&preview_mini, move |date| {
            displayed_day.set(date);
            refresh_preview();
        });
    }
    refresh_preview();

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
        let video_toggle = video_toggle.clone();
        let video_url_row = video_url_row.clone();
        let categorize_entry = categorize_entry.clone();
        let busy_dropdown = busy_dropdown.clone();
        let sensitivity_dropdown = sensitivity_dropdown.clone();
        let reminder_dropdown = reminder_dropdown.clone();
        let series = series.clone();
        let existing_meta = existing_meta.clone();
        let existing_attendees = existing_attendees.clone();
        let scope_dropdown = scope_dropdown.clone();
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
            let rrule = match &series.unrepresentable_raw {
                Some(raw) if series.current().is_none() => Some(raw.clone()),
                _ => series.current().map(|rule| recurrence::to_rrule_string(&rule)),
            };
            let input = FormInput {
                title: title_row.text().to_string(),
                location: location_row.text().to_string(),
                description: description_buffer.text(&description_buffer.start_iter(), &description_buffer.end_iter(), false).to_string(),
                all_day: all_day_switch.is_active(),
                start_local: date_time_from_form(&start_date, &start_hour, &start_minute, all_day_switch.is_active()),
                end_local: date_time_from_form(&end_date, &end_hour, &end_minute, all_day_switch.is_active()),
                attendee_tokens: attendees_field.addresses(),
                existing_attendees: existing_attendees.clone(),
                categories: categorize_entry.text().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                sensitivity: match sensitivity_dropdown.selected() {
                    1 => EventSensitivity::Private,
                    2 => EventSensitivity::Confidential,
                    _ => EventSensitivity::Public,
                },
                transparency: if busy_dropdown.selected() == 1 {
                    EventTransparency::Free
                } else {
                    EventTransparency::Busy
                },
                reminder_minutes_before: reminder_choices.get(reminder_dropdown.selected() as usize).and_then(|(_, m)| *m),
                video_enabled: video_toggle.is_active(),
                conference_url_text: video_url_row.text().to_string(),
                rrule,
                recurrence_id: None,
                recurrence_range: RecurrenceRange::default(),
                exdates: existing.as_ref().map(|occ| occ.exdates.clone()).unwrap_or_default(),
                owner_email: owner_email.clone(),
                existing_meta: existing_meta.clone(),
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
        let categorize_entry = categorize_entry.clone();
        let busy_dropdown = busy_dropdown.clone();
        let sensitivity_dropdown = sensitivity_dropdown.clone();
        let reminder_dropdown = reminder_dropdown.clone();
        let description_buffer = description_buffer.clone();
        let series = series.clone();
        print_button.connect_clicked(move |_| {
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
                &categorize_entry,
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
    }

    dialog.present();
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
        rrule: None,
        recurrence_id: None,
        exdates: Vec::new(),
        master_start: None,
        master_end: None,
        href: None,
        etag: None,
        master_href: None,
        master_etag: None,
        attendees: Vec::new(),
        organizer: None,
        categories: Vec::new(),
        sensitivity: EventSensitivity::default(),
        transparency: EventTransparency::default(),
        reminder_minutes_before: None,
        conference_url: None,
    }
}

/// The default span for a fresh event: from the next whole hour to an hour
/// later, both local naive times (23:xx rolls to 00:00 tomorrow).
fn default_event_times() -> (NaiveDateTime, NaiveDateTime) {
    let now = chrono::Local::now().naive_local();
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
fn date_time_from_form(date: &Rc<Cell<NaiveDate>>, hour: &gtk::SpinButton, minute: &gtk::SpinButton, all_day: bool) -> NaiveDateTime {
    let (h, m) = if all_day { (0, 0) } else { (hour.value() as u32, minute.value() as u32) };
    date.get().and_hms_opt(h, m, 0).unwrap_or(date.get().and_hms_opt(0, 0, 0).unwrap())
}

/// Local naive datetime -> UTC, resolving DST gaps/ambiguities deterministically
/// (a gap takes the pre-transition interpretation; an ambiguous time takes the
/// earlier offset) rather than panicking.
fn local_to_utc(naive: NaiveDateTime) -> DateTime<Utc> {
    use chrono::TimeZone;
    match chrono::Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.with_timezone(&Utc),
        chrono::LocalResult::Ambiguous(earlier, _later) => earlier.with_timezone(&Utc),
        chrono::LocalResult::None => chrono::Local.from_utc_datetime(&naive).with_timezone(&Utc),
    }
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
    categorize_entry: &gtk::Entry,
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
    let series_text = match &series.unrepresentable_raw {
        Some(raw) => raw.clone(),
        None => series.current().map(|rule| recurrence::describe(&rule)).unwrap_or_else(|| "Does not repeat".to_string()),
    };
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
        rows.push(("Video call", if url.is_empty() { "Yes".to_string() } else { url }));
    }
    let categories = categorize_entry.text().to_string();
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

/// The calendar's currently selected date, formatted for the date button's
/// label (e.g. "Wed, Aug 12").
fn format_calendar_date(cal: &gtk::Calendar) -> String {
    NaiveDate::from_ymd_opt(cal.year(), cal.month() as u32 + 1, cal.day() as u32)
        .map(|d| d.format("%a, %b %-d").to_string())
        .unwrap_or_default()
}

/// A compact date button (opening a popover with the full month grid) beside
/// an `hh:mm` spin-button pair, labeled above, for one end of an event's span.
fn time_cluster(label: &str, calendar: &gtk::Calendar, hour: &gtk::SpinButton, minute: &gtk::SpinButton) -> gtk::Box {
    let separator = gtk::Label::builder().label(":").css_classes(["dim-label"]).build();
    let time_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).valign(gtk::Align::Center).build();
    time_box.append(hour);
    time_box.append(&separator);
    time_box.append(minute);

    let date_label = gtk::Label::new(Some(&format_calendar_date(calendar)));
    let popover = gtk::Popover::builder().child(calendar).build();
    {
        let date_label = date_label.clone();
        let popover = popover.clone();
        calendar.connect_day_selected(move |cal| {
            date_label.set_label(&format_calendar_date(cal));
            popover.popdown();
        });
    }
    let date_button = gtk::MenuButton::builder().popover(&popover).child(&date_label).build();

    let cluster = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    cluster.append(&gtk::Label::builder().label(label).css_classes(["dim-label", "caption"]).xalign(0.0).build());
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    row.append(&date_button);
    row.append(&time_box);
    cluster.append(&row);
    cluster
}

/// Plain data extracted from the form's widgets, kept separate from the
/// widgets themselves so [`build_event_from_input`] is a pure function -
/// testable without a GTK display.
struct FormInput {
    title: String,
    location: String,
    description: String,
    all_day: bool,
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
    transparency: EventTransparency,
    reminder_minutes_before: Option<i64>,
    video_enabled: bool,
    conference_url_text: String,
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

pub fn calendar_event_from_occurrence(occ: &EventOccurrence, start_local: NaiveDateTime, end_local: NaiveDateTime) -> CalendarEvent {
    let model_end = if occ.all_day { end_local + chrono::Duration::days(1) } else { end_local };
    CalendarEvent {
        uid: occ.uid.clone(),
        calendar_id: occ.calendar_id.clone(),
        summary: occ.summary.clone(),
        description: occ.description.clone(),
        location: occ.location.clone(),
        start: local_to_utc(start_local),
        end: local_to_utc(model_end),
        all_day: occ.all_day,
        rrule: occ.rrule.clone(),
        recurrence_id: occ.recurrence_id,
        recurrence_range: RecurrenceRange::default(),
        exdates: occ.exdates.clone(),
        rdates: Vec::new(),
        href: occ.href.clone(),
        etag: occ.etag.clone(),
        attendees: occ.attendees.clone(),
        organizer: occ.organizer.clone(),
        categories: occ.categories.clone(),
        sensitivity: occ.sensitivity,
        transparency: occ.transparency,
        reminder_minutes_before: occ.reminder_minutes_before,
        conference_url: occ.conference_url.clone(),
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
            let status = input
                .existing_attendees
                .iter()
                .find(|a| a.address.address.eq_ignore_ascii_case(&address))
                .map(|a| a.status)
                .unwrap_or(AttendeeStatus::NeedsAction);
            Attendee {
                address: EmailAddress { name: name_of(token), address },
                role: AttendeeRole::Required,
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

    let (uid, href, etag) = input.existing_meta.unwrap_or_else(|| (EventUid(uuid::Uuid::new_v4().to_string()), None, None));

    let summary = input.title.trim();
    let location = input.location.trim();
    let description = input.description.trim();

    Ok(CalendarEvent {
        uid,
        calendar_id: CalendarId(String::new()),
        summary: (!summary.is_empty()).then(|| summary.to_string()),
        description: (!description.is_empty()).then(|| description.to_string()),
        location: (!location.is_empty()).then(|| location.to_string()),
        start: local_to_utc(input.start_local),
        end: local_to_utc(model_end),
        all_day: input.all_day,
        rrule: input.rrule,
        recurrence_id: input.recurrence_id,
        recurrence_range: input.recurrence_range,
        exdates: input.exdates,
        rdates: Vec::new(),
        href,
        etag,
        attendees,
        organizer,
        categories: input.categories,
        sensitivity: input.sensitivity,
        transparency: input.transparency,
        reminder_minutes_before: input.reminder_minutes_before,
        conference_url,
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
            start_local: start,
            end_local: start + chrono::Duration::hours(1),
            attendee_tokens: Vec::new(),
            existing_attendees: Vec::new(),
            categories: Vec::new(),
            sensitivity: EventSensitivity::Public,
            transparency: EventTransparency::Busy,
            reminder_minutes_before: None,
            video_enabled: false,
            conference_url_text: String::new(),
            rrule: None,
            recurrence_id: None,
            recurrence_range: RecurrenceRange::default(),
            exdates: Vec::new(),
            owner_email: None,
            existing_meta: None,
        }
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
            rrule: None,
            recurrence_id: recurrence_id.map(local_to_utc),
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
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
    fn omits_organizer_without_attendees_even_with_owner_email() {
        let mut input = base_input();
        input.owner_email = Some("me@example.com".to_string());
        let event = build_event_from_input(input).unwrap();
        assert!(event.organizer.is_none());
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
            rrule: None,
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: Some("https://dav.example.com/cal/evt-1.ics".to_string()),
            etag: Some("\"abc\"".to_string()),
            master_href: None,
            master_etag: None,
            attendees: vec![Attendee {
                address: EmailAddress::new("alice@example.com"),
                role: AttendeeRole::Required,
                status: AttendeeStatus::Accepted,
            }],
            organizer: Some(EmailAddress::new("me@example.com")),
            categories: vec!["team".to_string()],
            sensitivity: EventSensitivity::Private,
            transparency: EventTransparency::Free,
            reminder_minutes_before: Some(10),
            conference_url: Some("https://meet.example.com/x".to_string()),
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
        assert_eq!(event.transparency, EventTransparency::Free);
        assert_eq!(event.reminder_minutes_before, Some(10));
        assert_eq!(event.conference_url.as_deref(), Some("https://meet.example.com/x"));
        assert_eq!(event.start, local_to_utc(moved));
        assert_eq!(event.end, local_to_utc(moved + chrono::Duration::hours(2)));
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
            start: local_to_utc(start),
            end: local_to_utc(start + chrono::Duration::days(2)),
            all_day: true,
            rrule: None,
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: EventSensitivity::default(),
            transparency: EventTransparency::default(),
            reminder_minutes_before: None,
            conference_url: None,
        };
        let moved = NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(), chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        // The drag reports the last *day* (inclusive); the model stores the
        // day after.
        let event = calendar_event_from_occurrence(&occ, moved, moved + chrono::Duration::days(1));
        assert_eq!(event.start, local_to_utc(moved));
        assert_eq!(event.end, local_to_utc(moved + chrono::Duration::days(2)));
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
            rrule: Some("FREQ=WEEKLY".to_string()),
            recurrence_id: None,
            exdates: vec!["2026-08-19T09:00:00Z".parse().unwrap()],
            master_start: Some(instance),
            master_end: Some(instance + chrono::Duration::hours(1)),
            href: Some("https://dav.example.com/cal/master.ics".to_string()),
            etag: Some("\"m1\"".to_string()),
            master_href: None,
            master_etag: None,
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: EventSensitivity::default(),
            transparency: EventTransparency::default(),
            reminder_minutes_before: None,
            conference_url: None,
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
}
