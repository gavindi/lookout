//! The task editor dialog: a modal `adw::Window` form for creating and
//! editing one `VTODO` task, mirroring `event_editor`'s shape (data-in /
//! callback-out, no network access). The caller routes the finished task to
//! the owning account's session via `on_save`/`on_delete`.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike};
use lookout_core::{CalendarId, CalendarTask, TaskPriority, TaskStatus, TaskUid};

/// What to prefill the editor with: the pickable calendars (label + id),
/// which one is selected by default, and the task being edited (if any).
pub struct TaskEditorPrefill<'a> {
    /// `(display label, calendar id)` for the calendar picker, in picker
    /// order. Empty disables the picker (there's no writable calendar).
    pub calendars: &'a [(String, CalendarId)],
    pub default_calendar: CalendarId,
    /// The task being edited; `None` opens the blank "new task" form. While
    /// set, the calendar picker is locked to the task's own calendar.
    pub existing: Option<&'a CalendarTask>,
    /// Field values to seed a *create* form with (e.g. the mail toolbar's
    /// "Add as Task" button, prefilling summary/notes from the selected
    /// email). Ignored when `existing` is set - an edit always starts from
    /// the real task, never a caller-supplied seed.
    pub prefill: Option<&'a CalendarTask>,
    /// The shared category master list (item 2.1 / T-08), offered by the
    /// editor's Categorize picker. Empty when the user has none yet.
    pub categories: &'a [crate::categories::CategoryDef],
}

/// The delete callback's target: the calendar plus the task's identity and
/// resource (uid, href, etag) - the local store keys by uid, the CalDAV
/// session by href.
type DeleteCallback = Rc<dyn Fn(CalendarId, TaskUid, Option<String>, Option<String>)>;

/// Opens the task editor as a modal dialog. `on_save` fires with the chosen
/// calendar and the finished task (new or edited); `on_delete` fires with
/// the delete target (href/etag - the editor's form isn't consulted for a
/// delete). Both are responsible for routing to the owning account's
/// session - the dialog only builds and validates the task.
pub fn show_task_editor(
    window: &adw::ApplicationWindow,
    prefill: TaskEditorPrefill,
    on_save: impl Fn(CalendarId, CalendarTask) + 'static,
    on_delete: impl Fn(CalendarId, TaskUid, Option<String>, Option<String>) + 'static,
) {
    let on_save: Rc<dyn Fn(CalendarId, CalendarTask)> = Rc::new(on_save);
    let on_delete: DeleteCallback = Rc::new(on_delete);

    let existing: Option<CalendarTask> = prefill.existing.cloned();
    let has_existing = existing.is_some();
    let seed: Option<CalendarTask> = existing.clone().or_else(|| prefill.prefill.cloned());

    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title(if has_existing { "Edit task" } else { "New task" })
        .default_width(760)
        .default_height(660)
        .build();

    // --- Calendar picker (same StringList + parallel id vector convention
    // as the event editor).
    let calendar_labels: Vec<String> = prefill.calendars.iter().map(|(label, _)| label.clone()).collect();
    let calendar_ids: Vec<CalendarId> = prefill.calendars.iter().map(|(_, id)| id.clone()).collect();
    let default_index = calendar_ids.iter().position(|id| *id == prefill.default_calendar).unwrap_or(0) as u32;
    let label_refs: Vec<&str> = calendar_labels.iter().map(String::as_str).collect();
    let string_list = gtk::StringList::new(&label_refs);
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

    // --- Summary / notes.
    let summary_row = adw::EntryRow::builder().title("Summary").build();
    let notes_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(4)
        .right_margin(4)
        .top_margin(2)
        .bottom_margin(2)
        .build();
    let notes_label = gtk::Label::builder()
        .label("Notes")
        .css_classes(["dim-label", "caption"])
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    crate::privacy::mark_ui(&notes_label);
    let notes_scroller = gtk::ScrolledWindow::builder()
        .child(&notes_view)
        .vexpand(true)
        .min_content_height(220)
        .css_classes(["card"])
        .build();

    // --- Due date + time. A "Due" switch turns the whole cluster on/off -
    // an undated task (no `DUE` line at all) is the RFC-legal way to say
    // "whenever". The calendar lives in a popover behind a compact date
    // button - the same pattern as the event editor's time clusters, since a
    // `GtkCalendar` in the form flow expands to fill whatever space it's
    // given and stretches the rows around it. Date is tracked through an
    // `Rc<Cell<NaiveDate>>` updated on `day-selected` (GtkCalendar's getters
    // need v4_14, which this crate builds against); times are two spin
    // buttons, centered against the date button so they keep their natural
    // height.
    let due_switch_row = adw::SwitchRow::builder().title("Due").build();

    let due_date = Rc::new(Cell::new(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()));
    let due_calendar = gtk::Calendar::new();
    let due_hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
    let due_minute = gtk::SpinButton::with_range(0.0, 59.0, 1.0);
    for spin in [&due_hour, &due_minute] {
        spin.set_digits(0);
        spin.set_width_chars(2);
    }

    fn set_calendar_date(cal: &gtk::Calendar, tracked: &Rc<Cell<NaiveDate>>, date: NaiveDate) {
        tracked.set(date);
        if let Ok(dt) = gtk::glib::DateTime::from_utc(date.year(), date.month() as i32, date.day() as i32, 0, 0, 0.0) {
            cal.set_date(Some(&dt));
        }
    }

    let default_due = (Local::now() + Duration::days(1))
        .date_naive()
        .and_time(NaiveTime::from_hms_opt(9, 0, 0).unwrap_or(NaiveTime::MIN));
    let initial_due: Option<NaiveDateTime> = seed.as_ref().and_then(|t| t.due.map(|d| d.with_timezone(&Local).naive_local())).or(Some(default_due));
    let initial_due = initial_due.unwrap_or(default_due);
    set_calendar_date(&due_calendar, &due_date, initial_due.date());
    due_hour.set_value(f64::from(initial_due.hour()));
    due_minute.set_value(f64::from(initial_due.minute()));
    due_switch_row.set_active(seed.as_ref().is_some_and(|t| t.due.is_some()));

    let due_date_label = gtk::Label::new(Some(&format!("{}", initial_due.date().format("%a, %b %-d"))));
    let due_popover = gtk::Popover::builder().child(&due_calendar).build();
    {
        let due_date = due_date.clone();
        let due_date_label = due_date_label.clone();
        let due_popover = due_popover.clone();
        due_calendar.connect_day_selected(move |cal| {
            if let Some(date) = NaiveDate::from_ymd_opt(cal.year(), cal.month() as u32 + 1, cal.day() as u32) {
                due_date.set(date);
                due_date_label.set_label(&format!("{}", date.format("%a, %b %-d")));
            }
            due_popover.popdown();
        });
    }
    let due_date_button = gtk::MenuButton::builder().popover(&due_popover).child(&due_date_label).build();

    let time_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).valign(gtk::Align::Center).build();
    time_box.append(&due_hour);
    time_box.append(&gtk::Label::builder().label(":").css_classes(["dim-label"]).build());
    time_box.append(&due_minute);

    let datetime_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(8)
        .build();
    datetime_row.append(&due_date_button);
    datetime_row.append(&time_box);
    let due_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    due_box.append(&due_switch_row);
    due_box.append(&datetime_row);

    // --- Priority (RFC 5545 scale mapped to user labels) + % complete.
    let priority_choices: &[(&str, u8)] = &[("None", 0), ("High", 1), ("Medium", 5), ("Low", 9)];
    let priority_model = gtk::StringList::new(&priority_choices.iter().map(|(label, _)| *label).collect::<Vec<_>>());
    let priority_dropdown = gtk::DropDown::builder().model(&priority_model).valign(gtk::Align::Center).build();
    let existing_priority = seed.as_ref().map(|t| t.priority.0).unwrap_or(0);
    let priority_index = priority_choices.iter().position(|(_, raw)| *raw == existing_priority).unwrap_or(0) as u32;
    priority_dropdown.set_selected(priority_index);
    let priority_row = adw::ActionRow::builder().title("Priority").build();
    priority_row.add_suffix(&priority_dropdown);

    let percent_spin = gtk::SpinButton::with_range(0.0, 100.0, 5.0);
    percent_spin.set_digits(0);
    percent_spin.set_width_chars(4);
    percent_spin.set_value(f64::from(seed.as_ref().and_then(|t| t.percent_complete).unwrap_or(0)));
    let percent_row = adw::ActionRow::builder().title("% complete").build();
    percent_row.add_suffix(&percent_spin);

    // Categorize (item 2.1 / T-08): the shared master list as toggle rows.
    let (categorize_button, selected_categories) =
        crate::event_editor::build_category_picker(prefill.categories, seed.as_ref().map(|t| t.categories.as_slice()).unwrap_or(&[]), false);
    let categorize_row = adw::ActionRow::builder().title("Categorize").build();
    categorize_row.add_suffix(&categorize_button);

    // Reminder (T-05) and Repeat (T-06). Both are relative to the due date -
    // a task with no due date has nothing to anchor them to, so the rows
    // disable with the Due switch. Google Tasks has neither, so the rows are
    // hidden for a `googletasks:` list (the calendar picker drives that).
    let (reminder_labels, reminder_values, reminder_index) = task_reminder_choices(seed.as_ref().and_then(|t| t.reminder_minutes_before));
    let reminder_refs: Vec<&str> = reminder_labels.iter().map(String::as_str).collect();
    let reminder_model = gtk::StringList::new(&reminder_refs);
    let reminder_dropdown = gtk::DropDown::builder().model(&reminder_model).valign(gtk::Align::Center).build();
    reminder_dropdown.set_selected(reminder_index);
    crate::privacy::mark_content(&reminder_dropdown);
    let reminder_row = adw::ActionRow::builder().title("Remind me").build();
    reminder_row.add_suffix(&reminder_dropdown);

    // The series anchor is the due date (Lookout's only task date). Build the
    // shareable Series popover once and read it on Save.
    let series_start = seed
        .as_ref()
        .and_then(|t| t.due)
        .map(|due| due.with_timezone(&Local).date_naive())
        .unwrap_or_else(|| Local::now().date_naive());
    let series = crate::recurrence::build_series_control(seed.as_ref().and_then(|t| t.rrule.as_deref()), crate::message_list::current_week_start(), series_start);
    crate::privacy::mark_content(&series.button);
    series.button.set_tooltip_text(Some(&series.summary()));
    let repeat_row = adw::ActionRow::builder().title("Repeat").build();
    repeat_row.add_suffix(&series.button);

    // Keep the two schedule rows honest as the calendar/due state changes.
    let apply_schedule_state: Rc<dyn Fn()> = {
        let due_switch_row = due_switch_row.clone();
        let calendar_dropdown = calendar_dropdown.clone();
        let calendar_ids = calendar_ids.clone();
        let reminder_row = reminder_row.clone();
        let repeat_row = repeat_row.clone();
        let reminder_dropdown = reminder_dropdown.clone();
        let repeat_button = series.button.clone();
        Rc::new(move || {
            let selected = calendar_dropdown.selected() as usize;
            let google = calendar_ids.get(selected).is_some_and(|id| id.0.starts_with("googletasks:"));
            let due = due_switch_row.is_active();
            reminder_row.set_visible(!google);
            repeat_row.set_visible(!google);
            reminder_dropdown.set_sensitive(due);
            repeat_button.set_sensitive(due);
        })
    };
    {
        let apply = apply_schedule_state.clone();
        due_switch_row.connect_active_notify(move |_| apply());
    }
    {
        let apply = apply_schedule_state.clone();
        calendar_dropdown.connect_selected_notify(move |_| apply());
    }
    apply_schedule_state();

    let fields_group = adw::PreferencesGroup::new();
    fields_group.add(&calendar_row);
    fields_group.add(&summary_row);
    fields_group.add(&priority_row);
    fields_group.add(&percent_row);
    fields_group.add(&categorize_row);
    fields_group.add(&reminder_row);
    fields_group.add(&repeat_row);

    let error_label = gtk::Label::builder().wrap(true).xalign(0.0).halign(gtk::Align::Start).css_classes(["error"]).build();
    crate::privacy::mark_ui(&error_label);
    error_label.set_visible(false);

    let form_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .vexpand(true)
        .build();
    form_box.append(&fields_group);
    form_box.append(&due_box);
    form_box.append(&error_label);
    form_box.append(&notes_label);
    form_box.append(&notes_scroller);
    let form_scroller = gtk::ScrolledWindow::builder()
        .child(&form_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    // --- Top action row: Cancel/Save, spacer, Delete (existing only).
    let cancel_button = gtk::Button::with_label("Cancel");
    let save_button = gtk::Button::with_label(if has_existing { "Save" } else { "Add" });
    save_button.add_css_class("suggested-action");
    let delete_button = gtk::Button::from_icon_name("user-trash-symbolic");
    delete_button.add_css_class("destructive-action");
    delete_button.set_tooltip_text(Some("Delete"));
    delete_button.set_visible(has_existing);

    let top_bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    top_bar.append(&cancel_button);
    top_bar.append(&save_button);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    top_bar.append(&spacer);
    top_bar.append(&delete_button);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&top_bar);
    toolbar_view.set_content(Some(&form_scroller));
    dialog.set_content(Some(&toolbar_view));

    // --- Prefill the text fields from the edited task, or a caller-supplied
    // create seed.
    if let Some(task) = &seed {
        summary_row.set_text(task.summary.as_deref().unwrap_or(""));
        if let Some(description) = &task.description {
            notes_view.buffer().set_text(description);
        }
    }

    // --- Collect the form into a `CalendarTask`.
    let existing_meta: Option<(TaskUid, Option<String>, Option<String>)> = existing.as_ref().map(|t| (t.uid.clone(), t.href.clone(), t.etag.clone()));

    let save_handler = {
        let dialog = dialog.clone();
        let calendar_dropdown = calendar_dropdown.clone();
        let calendar_ids = calendar_ids.clone();
        let error_label = error_label.clone();
        let summary_row = summary_row.clone();
        let notes_view = notes_view.clone();
        let due_switch_row = due_switch_row.clone();
        let due_date = due_date.clone();
        let due_hour = due_hour.clone();
        let due_minute = due_minute.clone();
        let priority_dropdown = priority_dropdown.clone();
        let percent_spin = percent_spin.clone();
        let selected_categories = selected_categories.clone();
        let reminder_dropdown = reminder_dropdown.clone();
        let reminder_values = reminder_values.clone();
        let series = series;
        let existing_meta = existing_meta.clone();
        let on_save = on_save;
        move |_button: &gtk::Button| {
            let selected = calendar_dropdown.selected() as usize;
            let Some(calendar_id) = calendar_ids.get(selected).cloned() else {
                error_label.set_label("No calendar selected.");
                error_label.set_visible(true);
                return;
            };
            let summary = summary_row.text().trim().to_string();
            if summary.is_empty() {
                error_label.set_label("Give the task a summary.");
                error_label.set_visible(true);
                return;
            }
            let due = if due_switch_row.is_active() {
                let naive = due_date
                    .get()
                    .and_time(NaiveTime::from_hms_opt(due_hour.value() as u32, due_minute.value() as u32, 0).unwrap_or(NaiveTime::MIN));
                Some(
                    naive
                        .and_local_timezone(Local)
                        .single()
                        .unwrap_or_else(|| Local.from_utc_datetime(&naive))
                        .with_timezone(&chrono::Utc),
                )
            } else {
                None
            };
            let percent = percent_spin.value() as u8;
            let (status, completed, percent_complete) = if percent >= 100 {
                (
                    TaskStatus::Completed,
                    existing.as_ref().and_then(|t| t.completed).or_else(|| Some(chrono::Utc::now())),
                    Some(100),
                )
            } else {
                (TaskStatus::NeedsAction, None, if percent == 0 { None } else { Some(percent) })
            };
            let (uid, href, etag) = match &existing_meta {
                Some((uid, href, etag)) => (uid.clone(), href.clone(), etag.clone()),
                None => (TaskUid(uuid::Uuid::new_v4().to_string()), None, None),
            };
            // Google Tasks has no recurrence or reminder API. Its rows are
            // hidden, and a save into such a list clears both rather than
            // pretending they were stored.
            let google_calendar = calendar_id.0.starts_with("googletasks:");
            let due_active = due_switch_row.is_active();
            let rrule = if due_active && !google_calendar { series.rrule(false) } else { None };
            let reminder_minutes_before = if due_active && !google_calendar {
                reminder_values.get(reminder_dropdown.selected() as usize).copied().flatten()
            } else {
                None
            };
            // A recurring task keeps the original series anchor (`DTSTART`);
            // a brand-new series is anchored on the first due date.
            let start = if rrule.is_some() {
                existing.as_ref().filter(|task| task.rrule.is_some()).and_then(|task| task.start).or(due)
            } else {
                existing.as_ref().and_then(|t| t.start)
            };
            let mut task = CalendarTask {
                uid,
                calendar_id: calendar_id.clone(),
                summary: Some(summary),
                description: {
                    let buffer = notes_view.buffer();
                    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
                    if text.trim().is_empty() {
                        None
                    } else {
                        Some(text)
                    }
                },
                due,
                start,
                rrule,
                // A new task is stamped now so it can be sorted by creation;
                // an edit keeps whatever the server gave us (never over-write).
                created: existing.as_ref().and_then(|t| t.created).or_else(|| existing.is_none().then(chrono::Utc::now)),
                completed,
                status,
                priority: TaskPriority(priority_choices.get(priority_dropdown.selected() as usize).map(|(_, raw)| *raw).unwrap_or(0)),
                percent_complete,
                reminder_minutes_before,
                categories: selected_categories.borrow().clone(),
                href,
                etag,
            };
            // Refuse a rule Microsoft To Do can't store *before* submitting -
            // Graph's structured recurrence is a strict subset of RFC 5545
            // (e.g. no BYSETPOS first/last-weekday form).
            if calendar_id.0.starts_with("todo:") && !lookout_graph::todo::todo_recurrence_supported(&task) {
                error_label.set_label("Microsoft To Do can't store this repeat pattern. Choose a daily, weekly, monthly or yearly rule.");
                error_label.set_visible(true);
                return;
            }
            // Setting % complete to 100 is the editor's completion route; roll
            // a recurring task just like the list's checkbox does.
            crate::task_recurrence::roll_forward_if_completed(existing.as_ref(), &mut task);
            on_save(calendar_id, task);
            dialog.close();
        }
    };
    save_button.connect_clicked(save_handler);

    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }

    if has_existing {
        let dialog = dialog.clone();
        let calendar_dropdown = calendar_dropdown.clone();
        let calendar_ids = calendar_ids.clone();
        let error_label = error_label.clone();
        let existing_meta = existing_meta.clone();
        let on_delete = on_delete;
        delete_button.connect_clicked(move |_| {
            let selected = calendar_dropdown.selected() as usize;
            let Some(calendar_id) = calendar_ids.get(selected).cloned() else {
                error_label.set_label("No calendar selected.");
                error_label.set_visible(true);
                return;
            };
            if let Some((uid, href, etag)) = &existing_meta {
                on_delete(calendar_id, uid.clone(), href.clone(), etag.clone());
                dialog.close();
            }
        });
    }

    dialog.present();
}

/// The Remind-me dropdown's `(labels, minutes, selected index)`. Uses the
/// shared calendar reminder presets with task wording, and keeps a value that
/// isn't one of the presets (e.g. an arbitrary Graph `reminderDateTime`) as a
/// synthetic "N minutes before/after" choice so editing a task never silently
/// drops its reminder.
fn task_reminder_choices(existing: Option<i64>) -> (Vec<String>, Vec<Option<i64>>, u32) {
    let mut labels: Vec<String> = crate::settings::REMINDER_CHOICES
        .iter()
        .map(|(label, minutes)| if *minutes == Some(0) { "At time of task".to_string() } else { (*label).to_string() })
        .collect();
    let mut values: Vec<Option<i64>> = crate::settings::REMINDER_CHOICES.iter().map(|(_, minutes)| *minutes).collect();
    let mut index = existing.and_then(|minutes| values.iter().position(|value| *value == Some(minutes))).unwrap_or(0) as u32;
    if let Some(minutes) = existing {
        if !values.contains(&Some(minutes)) {
            labels.push(match minutes {
                0 => "At time of task".to_string(),
                before if before > 0 => format!("{before} minutes before"),
                after => format!("{} minutes after", -after),
            });
            values.push(Some(minutes));
            index = (labels.len() - 1) as u32;
        }
    }
    (labels, values, index)
}

#[cfg(test)]
mod tests {
    use super::task_reminder_choices;

    #[test]
    fn task_reminder_choices_use_task_wording_and_default_to_none() {
        let (labels, values, index) = task_reminder_choices(None);
        assert_eq!(index, 0);
        assert_eq!(values[0], None);
        assert!(labels.iter().any(|label| label == "At time of task"));
        assert!(!labels.iter().any(|label| label.contains("event")));
    }

    #[test]
    fn custom_task_reminders_survive_the_editor() {
        let (labels, values, index) = task_reminder_choices(Some(45));
        assert_eq!(values[index as usize], Some(45));
        assert_eq!(labels[index as usize], "45 minutes before");

        let (labels, values, index) = task_reminder_choices(Some(-10));
        assert_eq!(values[index as usize], Some(-10));
        assert_eq!(labels[index as usize], "10 minutes after");
    }
}
