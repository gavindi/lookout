//! Modal event editor (create/edit/delete) for the Calendar module - the
//! counterpart of `compose.rs`'s mail composer, in dialog form since the
//! calendar has no reading-pane slot to host an inline editor.
//!
//! Data-in/widget-state-out like the rest of the calendar code: the caller
//! supplies the calendars a new event could land in, which one to preselect,
//! and (for an edit) the occurrence being changed, and registers two callbacks
//! - `on_save(CalendarId, CalendarEvent)` for both create and edit (the create
//!   vs update distinction falls out of the event's href/etag), and
//!   `on_delete(CalendarId, EventUid, href, etag)` - which the dialog never
//!   talks to the network to fulfil itself; the caller routes the produced
//!   [`CalendarEvent`] to the owning account's session.

use std::cell::Cell;
use std::rc::Rc;

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike, Utc};
use gtk::prelude::*;
use lookout_core::{CalendarEvent, CalendarId, EventOccurrence, EventUid};

/// The save callback's target: the picked calendar plus the finished event.
pub type SaveCallback = Rc<dyn Fn(CalendarId, CalendarEvent)>;
/// The delete callback's target: the calendar plus the resource to remove.
type DeleteCallback = Rc<dyn Fn(CalendarId, EventUid, Option<String>, Option<String>)>;
/// The edited occurrence's identity + write target, threaded through the form.
type EventMeta = (EventUid, Option<String>, Option<String>, Option<String>);

/// What to prefill the editor with: the pickable calendars (label + id), which
/// calendar is selected by default, and the event being edited (if any).
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
}

/// Opens the event editor as a modal dialog. `on_save` fires with the chosen
/// calendar and the finished event (new or edited); `on_delete` fires with the
/// delete target. Both are responsible for routing to the owning account's
/// session - the dialog only builds and validates the event.
pub fn show_event_editor(
    window: &adw::ApplicationWindow,
    prefill: EventEditorPrefill,
    on_save: impl Fn(CalendarId, CalendarEvent) + 'static,
    on_delete: impl Fn(CalendarId, EventUid, Option<String>, Option<String>) + 'static,
) {
    // Boxed as trait objects: a closure capturing a generic `impl Fn`
    // parameter can't implement `Fn` itself (its own `call` needs the captured
    // generic's bound to line up with a late-bound lifetime), so `Rc<dyn Fn>`
    // gives the handler closures a concrete, nameable type to capture.
    let on_save: SaveCallback = Rc::new(on_save);
    let on_delete: DeleteCallback = Rc::new(on_delete);

    let existing: Option<EventOccurrence> = prefill.existing.cloned();
    let has_existing = existing.is_some();

    let dialog = gtk::Window::builder()
        .transient_for(window)
        .modal(true)
        .title(if has_existing { "Edit event" } else { "New event" })
        .default_width(560)
        .build();

    // --- Calendar picker: a `StringList` of labels plus a parallel id vector,
    // so the selected index maps straight back to a `CalendarId`.
    let calendar_labels: Vec<String> = prefill.calendars.iter().map(|(label, _)| label.clone()).collect();
    let calendar_ids: Vec<CalendarId> = prefill.calendars.iter().map(|(_, id)| id.clone()).collect();
    let default_index = calendar_ids.iter().position(|id| *id == prefill.default_calendar).unwrap_or(0) as u32;
    let label_refs: Vec<&str> = calendar_labels.iter().map(String::as_str).collect();
    let string_list = gtk::StringList::new(&label_refs);
    let calendar_dropdown = gtk::DropDown::builder()
        .model(&string_list)
        .selected(if calendar_ids.is_empty() { u32::MAX } else { default_index })
        .sensitive(!calendar_ids.is_empty() && !has_existing)
        .build();

    // --- Title / all-day toggle.
    let title_entry = gtk::Entry::builder().placeholder_text("Add a title").hexpand(true).build();
    let all_day_switch = gtk::Switch::new();
    all_day_switch.set_valign(gtk::Align::Center);
    all_day_switch.set_halign(gtk::Align::Start);

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
    // metadata-only edit can't silently re-anchor the whole series.
    let (initial_start, initial_end) = existing
        .as_ref()
        .map(|occ| {
            if occ.rrule.is_some() {
                if let (Some(master_start), Some(master_end)) = (occ.master_start, occ.master_end) {
                    return (
                        master_start.with_timezone(&chrono::Local).naive_local(),
                        master_end.with_timezone(&chrono::Local).naive_local(),
                    );
                }
            }
            (occ.start.with_timezone(&chrono::Local).naive_local(), occ.end.with_timezone(&chrono::Local).naive_local())
        })
        .or_else(|| prefill.suggested_start.map(|start| (start, start + chrono::Duration::hours(1))))
        .unwrap_or_else(default_event_times);
    set_calendar_date(&start_calendar, &start_date, initial_start.date());
    set_calendar_date(&end_calendar, &end_date, initial_end.date());
    wire_calendar(&start_calendar, &start_date);
    wire_calendar(&end_calendar, &end_date);
    start_hour.set_value(f64::from(initial_start.hour()));
    start_minute.set_value(f64::from(initial_start.minute()));
    end_hour.set_value(f64::from(initial_end.hour()));
    end_minute.set_value(f64::from(initial_end.minute()));

    let existing_all_day = existing.as_ref().map(|occ| occ.all_day).unwrap_or(false);
    all_day_switch.set_active(existing_all_day);
    for spin in [&start_hour, &start_minute, &end_hour, &end_minute] {
        spin.set_sensitive(!existing_all_day);
    }
    {
        let time_spins = [start_hour.clone(), start_minute.clone(), end_hour.clone(), end_minute.clone()];
        all_day_switch.connect_active_notify(move |switch| {
            let on = switch.is_active();
            for spin in &time_spins {
                spin.set_sensitive(!on);
            }
        });
    }

    // --- Location + description.
    let location_entry = gtk::Entry::builder().placeholder_text("Add a location").build();
    let description_buffer = gtk::TextBuffer::new(None);
    let description_view = gtk::TextView::builder()
        .buffer(&description_buffer)
        .wrap_mode(gtk::WrapMode::WordChar)
        .height_request(90)
        .build();

    // --- Form grid.
    let grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(8)
        .margin_top(12)
        .margin_bottom(4)
        .margin_start(12)
        .margin_end(12)
        .build();
    grid.attach(&form_label("Title"), 0, 0, 1, 1);
    grid.attach(&title_entry, 1, 0, 1, 1);
    grid.attach(&form_label("Calendar"), 0, 1, 1, 1);
    grid.attach(&calendar_dropdown, 1, 1, 1, 1);
    grid.attach(&form_label("All day"), 0, 2, 1, 1);
    grid.attach(&all_day_switch, 1, 2, 1, 1);
    grid.attach(&form_label("Starts"), 0, 3, 1, 1);
    grid.attach(&time_cluster(&start_calendar, &start_hour, &start_minute), 1, 3, 1, 1);
    grid.attach(&form_label("Ends"), 0, 4, 1, 1);
    grid.attach(&time_cluster(&end_calendar, &end_hour, &end_minute), 1, 4, 1, 1);
    grid.attach(&form_label("Location"), 0, 5, 1, 1);
    grid.attach(&location_entry, 1, 5, 1, 1);
    grid.attach(&form_label("Notes"), 0, 6, 1, 1);
    grid.attach(&description_view, 1, 6, 1, 1);

    // --- Recurring-series note + inline validation error.
    let recurring_note = gtk::Label::builder()
        .label("This event repeats - changes apply to the whole series.")
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .margin_top(4)
        .build();
    recurring_note.set_visible(existing.as_ref().is_some_and(|occ| occ.rrule.is_some()));
    let error_label = gtk::Label::builder().wrap(true).xalign(0.0).halign(gtk::Align::Start).margin_top(4).build();
    error_label.set_visible(false);

    // --- Buttons. Delete only exists for an edit.
    let cancel_button = gtk::Button::with_label("Cancel");
    let save_button = gtk::Button::with_label(if has_existing { "Save" } else { "Add" });
    save_button.add_css_class("suggested-action");
    let delete_button = gtk::Button::with_label("Delete");
    delete_button.add_css_class("destructive-action");
    delete_button.set_visible(has_existing);

    let button_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .margin_top(8)
        .margin_bottom(12)
        .build();
    if has_existing {
        button_row.append(&delete_button);
    }
    button_row.append(&cancel_button);
    button_row.append(&save_button);

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    content.append(&grid);
    content.append(&recurring_note);
    content.append(&error_label);
    content.append(&button_row);
    dialog.set_child(Some(&content));

    // --- Prefill text fields from the edited occurrence.
    if let Some(occ) = &existing {
        title_entry.set_text(occ.summary.as_deref().unwrap_or(""));
        location_entry.set_text(occ.location.as_deref().unwrap_or(""));
        if let Some(description) = &occ.description {
            description_buffer.set_text(description);
        }
    }

    // --- Collect the form into a `CalendarEvent`. The recurring
    // uid/href/etag/rrule come from the edited occurrence; a fresh event gets
    // a new UUID and no recurrence. calendar_id is a placeholder here and is
    // filled in by the caller-facing handlers with the picked calendar.
    let existing_meta = existing.map(|occ| (occ.uid.clone(), occ.rrule.clone(), occ.href.clone(), occ.etag.clone()));

    let save_handler = {
        let dialog = dialog.clone();
        let calendar_dropdown = calendar_dropdown.clone();
        let calendar_ids = calendar_ids.clone();
        let error_label = error_label.clone();
        let title_entry = title_entry.clone();
        let location_entry = location_entry.clone();
        let description_buffer = description_buffer.clone();
        let all_day_switch = all_day_switch.clone();
        let start_date = start_date.clone();
        let end_date = end_date.clone();
        let start_hour = start_hour.clone();
        let start_minute = start_minute.clone();
        let end_hour = end_hour.clone();
        let end_minute = end_minute.clone();
        let existing_meta = existing_meta.clone();
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
            match collect_event_from_form(
                &title_entry,
                &location_entry,
                &description_buffer,
                &all_day_switch,
                &start_date,
                &start_hour,
                &start_minute,
                &end_date,
                &end_hour,
                &end_minute,
                &existing_meta,
            ) {
                Ok(mut event) => {
                    event.calendar_id = calendar_id.clone();
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
        let existing_meta = existing_meta.clone();
        delete_button.connect_clicked(move |_| {
            let selected = calendar_dropdown.selected() as usize;
            let Some(calendar_id) = calendar_ids.get(selected).cloned() else {
                error_label.set_label("No calendar selected.");
                error_label.set_visible(true);
                return;
            };
            // Delete doesn't validate the form's times - the user may have
            // left the fields in a broken state and still want the event gone.
            if let Some((uid, _rrule, href, etag)) = &existing_meta {
                on_delete(calendar_id, uid.clone(), href.clone(), etag.clone());
                dialog.close();
            }
        });
    }

    dialog.present();
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
/// local naive datetime. All-day events are pinned to 00:00 (their model end
/// is the exclusive next-day boundary, matching `CalendarEvent`).
fn date_time_from_form(date: &Rc<Cell<NaiveDate>>, hour: &gtk::SpinButton, minute: &gtk::SpinButton, all_day: bool) -> NaiveDateTime {
    let (h, m) = if all_day { (0, 0) } else { (hour.value() as u32, minute.value() as u32) };
    date.get().and_hms_opt(h, m, 0).unwrap_or(date.get().and_hms_opt(0, 0, 0).unwrap())
}

/// Collects the form widgets into a [`CalendarEvent`]. `existing_meta` carries
/// the edited occurrence's uid/rrule/href/etag (a fresh event gets a new UUID
/// and no recurrence). `calendar_id` is left as a placeholder the caller fills
/// with the picked calendar. `Err` on an impossible time range, which the
/// caller surfaces in the dialog's error line.
#[allow(clippy::too_many_arguments)]
fn collect_event_from_form(
    title_entry: &gtk::Entry,
    location_entry: &gtk::Entry,
    description_buffer: &gtk::TextBuffer,
    all_day_switch: &gtk::Switch,
    start_date: &Rc<Cell<NaiveDate>>,
    start_hour: &gtk::SpinButton,
    start_minute: &gtk::SpinButton,
    end_date: &Rc<Cell<NaiveDate>>,
    end_hour: &gtk::SpinButton,
    end_minute: &gtk::SpinButton,
    existing_meta: &Option<EventMeta>,
) -> Result<CalendarEvent, String> {
    let all_day = all_day_switch.is_active();
    let start_local = date_time_from_form(start_date, start_hour, start_minute, all_day);
    let end_local = date_time_from_form(end_date, end_hour, end_minute, all_day);
    if end_local <= start_local {
        return Err("The event's end must be after its start.".to_string());
    }

    let summary = title_entry.text().to_string();
    let location = location_entry.text().to_string();
    let description = description_buffer.text(&description_buffer.start_iter(), &description_buffer.end_iter(), false).to_string();

    let (uid, rrule, href, etag) = existing_meta.clone().unwrap_or_else(|| (EventUid(uuid::Uuid::new_v4().to_string()), None, None, None));

    Ok(CalendarEvent {
        uid,
        calendar_id: CalendarId(String::new()),
        summary: if summary.trim().is_empty() { None } else { Some(summary.trim().to_string()) },
        description: if description.trim().is_empty() { None } else { Some(description.trim().to_string()) },
        location: if location.trim().is_empty() { None } else { Some(location.trim().to_string()) },
        start: local_to_utc(start_local),
        end: local_to_utc(end_local),
        all_day,
        rrule,
        href,
        etag,
    })
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

/// A right-aligned form label.
fn form_label(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).halign(gtk::Align::End).build()
}

/// A date calendar beside an `hh:mm` spin-button pair, for one end of an event.
fn time_cluster(calendar: &gtk::Calendar, hour: &gtk::SpinButton, minute: &gtk::SpinButton) -> gtk::Box {
    let separator = gtk::Label::builder().label(":").css_classes(["dim-label"]).build();
    let time_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).valign(gtk::Align::Center).build();
    time_box.append(hour);
    time_box.append(&separator);
    time_box.append(minute);

    let cluster = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    cluster.append(calendar);
    cluster.append(&time_box);
    cluster
}
