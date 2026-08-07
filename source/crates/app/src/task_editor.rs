//! The task editor dialog: a modal `adw::Window` form for creating and
//! editing one `VTODO` task, mirroring `event_editor`'s shape (data-in /
//! callback-out, no network access). The caller routes the finished task to
//! the owning account's session via `on_save`/`on_delete`.

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
}

/// The delete callback's target: the calendar plus the resource to remove.
type DeleteCallback = Rc<dyn Fn(CalendarId, Option<String>, Option<String>)>;

/// Opens the task editor as a modal dialog. `on_save` fires with the chosen
/// calendar and the finished task (new or edited); `on_delete` fires with
/// the delete target (href/etag - the editor's form isn't consulted for a
/// delete). Both are responsible for routing to the owning account's
/// session - the dialog only builds and validates the task.
pub fn show_task_editor(
    window: &adw::ApplicationWindow,
    prefill: TaskEditorPrefill,
    on_save: impl Fn(CalendarId, CalendarTask) + 'static,
    on_delete: impl Fn(CalendarId, Option<String>, Option<String>) + 'static,
) {
    let on_save: Rc<dyn Fn(CalendarId, CalendarTask)> = Rc::new(on_save);
    let on_delete: DeleteCallback = Rc::new(on_delete);

    let existing: Option<CalendarTask> = prefill.existing.cloned();
    let has_existing = existing.is_some();

    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title(if has_existing { "Edit task" } else { "New task" })
        .default_width(560)
        .default_height(620)
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
    let notes_scroller = gtk::ScrolledWindow::builder()
        .child(&notes_view)
        .vexpand(true)
        .min_content_height(110)
        .css_classes(["card"])
        .build();

    // --- Due date + time. A "Due" switch turns the whole cluster on/off -
    // an undated task (no `DUE` line at all) is the RFC-legal way to say
    // "whenever". Date is tracked through an `Rc<Cell<NaiveDate>>` updated
    // on `day-selected` (GtkCalendar's getters need v4_14, which this crate
    // builds against), times are two spin buttons.
    let due_switch = gtk::Switch::new();
    let due_switch_row = adw::ActionRow::builder().title("Due").build();
    due_switch_row.set_activatable_widget(Some(&due_switch));
    due_switch_row.add_suffix(&due_switch);

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
    fn wire_calendar(cal: &gtk::Calendar, tracked: &Rc<Cell<NaiveDate>>) {
        let tracked = tracked.clone();
        cal.connect_day_selected(move |cal| {
            if let Some(date) = NaiveDate::from_ymd_opt(cal.year(), cal.month() as u32 + 1, cal.day() as u32) {
                tracked.set(date);
            }
        });
    }

    let default_due = (Local::now() + Duration::days(1))
        .date_naive()
        .and_time(NaiveTime::from_hms_opt(9, 0, 0).unwrap_or(NaiveTime::MIN));
    let initial_due: Option<NaiveDateTime> = existing.as_ref().and_then(|t| t.due.map(|d| d.with_timezone(&Local).naive_local())).or(Some(default_due));
    let initial_due = initial_due.unwrap_or(default_due);
    set_calendar_date(&due_calendar, &due_date, initial_due.date());
    wire_calendar(&due_calendar, &due_date);
    due_hour.set_value(f64::from(initial_due.hour()));
    due_minute.set_value(f64::from(initial_due.minute()));
    due_switch.set_active(existing.as_ref().is_some_and(|t| t.due.is_some()));

    let datetime_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    let time_cluster = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).css_classes(["linked"]).build();
    time_cluster.append(&due_hour);
    time_cluster.append(&due_minute);
    datetime_row.append(&due_calendar);
    datetime_row.append(&time_cluster);
    let due_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(8)
        .build();
    due_box.append(&due_switch_row);
    due_box.append(&datetime_row);

    // --- Priority (RFC 5545 scale mapped to user labels) + % complete.
    let priority_choices: &[(&str, u8)] = &[("None", 0), ("High", 1), ("Medium", 5), ("Low", 9)];
    let priority_model = gtk::StringList::new(&priority_choices.iter().map(|(label, _)| *label).collect::<Vec<_>>());
    let priority_dropdown = gtk::DropDown::builder().model(&priority_model).valign(gtk::Align::Center).build();
    let existing_priority = existing.as_ref().map(|t| t.priority.0).unwrap_or(0);
    let priority_index = priority_choices.iter().position(|(_, raw)| *raw == existing_priority).unwrap_or(0) as u32;
    priority_dropdown.set_selected(priority_index);
    let priority_row = adw::ActionRow::builder().title("Priority").build();
    priority_row.add_suffix(&priority_dropdown);

    let percent_spin = gtk::SpinButton::with_range(0.0, 100.0, 5.0);
    percent_spin.set_digits(0);
    percent_spin.set_width_chars(4);
    percent_spin.set_value(f64::from(existing.as_ref().and_then(|t| t.percent_complete).unwrap_or(0)));
    let percent_row = adw::ActionRow::builder().title("% complete").build();
    percent_row.add_suffix(&percent_spin);

    let fields_group = adw::PreferencesGroup::new();
    fields_group.add(&calendar_row);
    fields_group.add(&summary_row);
    fields_group.add(&priority_row);
    fields_group.add(&percent_row);

    let error_label = gtk::Label::builder().wrap(true).xalign(0.0).halign(gtk::Align::Start).css_classes(["error"]).build();
    error_label.set_visible(false);

    let form_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
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

    // --- Prefill the text fields from the edited task.
    if let Some(task) = &existing {
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
        let due_switch = due_switch.clone();
        let due_date = due_date.clone();
        let due_hour = due_hour.clone();
        let due_minute = due_minute.clone();
        let priority_dropdown = priority_dropdown.clone();
        let percent_spin = percent_spin.clone();
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
            let due = if due_switch.is_active() {
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
            let task = CalendarTask {
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
                start: existing.as_ref().and_then(|t| t.start),
                completed,
                status,
                priority: TaskPriority(priority_choices.get(priority_dropdown.selected() as usize).map(|(_, raw)| *raw).unwrap_or(0)),
                percent_complete,
                categories: existing.as_ref().map(|t| t.categories.clone()).unwrap_or_default(),
                href,
                etag,
            };
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
            if let Some((_uid, href, etag)) = &existing_meta {
                on_delete(calendar_id, href.clone(), etag.clone());
                dialog.close();
            }
        });
    }
}
