//! Microsoft Graph ⇄ `lookout-core` Microsoft To Do mapping (OUTLOOK_PARITY.md
//! §5.5/§5.6, item 5.3 / A-05, T-12).
//!
//! Graph's `/me/todo/lists` and `/me/todo/lists/{id}/tasks` speak a JSON task
//! vocabulary, and the Tasks view speaks `lookout_core`'s [`CalendarTask`]. This
//! module is the one place the two are translated, so neither the session actor
//! nor the feature code hard-codes Graph property names (§10 "Interop strings").
//! Like `calendar` and `contacts`, it is deliberately pure - no HTTP, no
//! `GraphClient` - so every rule is unit-testable against inline JSON. The HTTP
//! transport lives in [`client`] (`TodoClient`), the actor in [`session`].
//!
//! Microsoft To Do is a *separate* backend from Graph calendars (item 5.1):
//! Graph calendars never carry VTODOs, so a To Do list maps to a synthetic
//! `todo:<list id>` [`CalendarId`], exactly as Google Tasks uses
//! `googletasks:<list id>` (`google_tasks.rs`). The Tasks view's merge/colour
//! machinery then treats the lists like calendars.
//!
//! Deliberately *not* carried, and why:
//! - **Checklist items** (`checklistItems`): the shared `CalendarTask` model has
//!   no steps field - subtasks are `T-07` / item 4.11. The mapper exposes
//!   [`ChecklistStep`] so the transport is ready, but a task round-trips without
//!   them; Graph's `PATCH` is partial, so Outlook's checklist items are never
//!   clobbered.
//!
//! Recurrence (`recurrence`, T-06) and reminders (`reminderDateTime` /
//! `isReminderOn`, T-05) are carried: Graph's structured recurrence maps
//! through the calendar module's `rrule_to_graph`/`recurrence_from_graph`, and
//! an absolute Graph reminder becomes the model's "minutes before due". A
//! reminder on a task with no due date is dropped (there is nothing to anchor
//! it to), and a rule Graph cannot express is refused by the client rather
//! than silently stored without recurrence.
//!
//! The flagged-email list (`linkedResources`) is *not* surfaced as its own
//! source: Outlook's flagged mail already reaches Lookout through the M-10
//! per-message `followupFlag` sync (`graph_mail.rs`, §4.2), so re-reading
//! Graph's `flaggedEmail` list would duplicate it. [`LinkedResource`] and
//! [`linked_resource_from_graph`] exist so that mapping is in one place.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
pub mod client;
pub mod session;

pub use client::TodoClient;

use chrono::{DateTime, NaiveDate, Utc};
use lookout_core::{CalendarId, CalendarTask, TaskPriority, TaskStatus, TaskUid};
use serde_json::{json, Value};

use crate::calendar::recurrence::{recurrence_from_graph, rrule_to_graph};
use crate::calendar::{graph_date_time, parse_graph_date_time};

/// The synthetic [`CalendarId`] prefix a Microsoft To Do list maps to.
pub const GRAPH_TODO_PREFIX: &str = "todo:";

/// The `/me/todo/lists/{id}` path segment for the built-in default list
/// ("Tasks") - Graph's `well-known list name`.
pub const WELL_KNOWN_DEFAULT_LIST: &str = "defaultList";
/// The `/me/todo/lists/{id}` path segment for the built-in flagged-email list.
pub const WELL_KNOWN_FLAGGED_EMAIL: &str = "flaggedEmail";

/// One Microsoft To Do list (`/me/todo/lists`): the Tasks view's "calendar"
/// for colour/grouping purposes. `id` is the Graph list id - the write target
/// and the synthetic calendar id's suffix. `well_known` is Graph's
/// `wellknownListName` (`defaultList`, `flaggedEmails`, `unknown`), or `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoList {
    pub id: String,
    pub display_name: String,
    pub well_known: Option<String>,
}

/// One `checklistItem` on a To Do task (a "step"). Not part of the shared
/// `CalendarTask` yet (T-07 / item 4.11); captured here so the transport and its
/// tests are ready when the model grows a steps field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecklistStep {
    pub id: Option<String>,
    pub display_name: String,
    pub is_checked: bool,
}

/// One `linkedResource` - a To Do task's link back to the message (or other
/// item) it was created from. The flagged-email list uses these to point at
/// flagged messages; Lookout's M-10 flag sync already covers that, so this is
/// mapped for completeness rather than surfaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedResource {
    pub id: Option<String>,
    pub web_url: Option<String>,
    pub display_name: Option<String>,
    pub application_name: Option<String>,
    /// The source item's own id (for a flagged message, Graph's message id).
    pub external_id: Option<String>,
}

/// The synthetic calendar id one To Do list maps to.
pub fn graph_todo_calendar_id(list_id: &str) -> CalendarId {
    CalendarId(format!("{GRAPH_TODO_PREFIX}{list_id}"))
}

/// Extracts the list id from a [`graph_todo_calendar_id`].
pub fn graph_todo_list_id(calendar_id: &CalendarId) -> Option<&str> {
    calendar_id.0.strip_prefix(GRAPH_TODO_PREFIX)
}

/// Maps a Graph `todoTaskList` to a [`TodoList`], or `None` when it has no id.
/// A nameless list falls back to "Tasks" so the picker never renders blank.
pub fn todo_list_from_graph(value: &Value) -> Option<TodoList> {
    let id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
    let display_name = value
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Tasks")
        .to_string();
    Some(TodoList {
        id: id.to_string(),
        display_name,
        well_known: non_empty_str(value, "wellknownListName"),
    })
}

/// Maps a Graph `todoTask` to a [`CalendarTask`] in `list_id`'s synthetic
/// calendar. A task with no id is `None` (nothing to key a write on).
///
/// Graph's three-state `importance` maps onto the model's RFC 5545 `PRIORITY`
/// scale lossily (high→1, normal→0, low→9); Graph has no percentage, so
/// `percent_complete` is only `Some(100)` for a completed task.
pub fn todo_task_from_graph(value: &Value, list_id: &str) -> Option<CalendarTask> {
    let id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
    let status = status_from_graph(value.get("status").and_then(Value::as_str));
    let due = value.get("dueDateTime").and_then(parse_graph_date_time).map(|(instant, _)| instant);
    Some(CalendarTask {
        uid: TaskUid(id.to_string()),
        calendar_id: graph_todo_calendar_id(list_id),
        summary: non_empty_str(value, "title"),
        description: body_text(value),
        due,
        start: value.get("startDateTime").and_then(parse_graph_date_time).map(|(instant, _)| instant),
        rrule: value.get("recurrence").and_then(recurrence_from_graph),
        created: value.get("createdDateTime").and_then(parse_iso8601),
        completed: value.get("completedDateTime").and_then(parse_graph_date_time).map(|(instant, _)| instant),
        status,
        priority: priority_from_graph(value.get("importance").and_then(Value::as_str)),
        percent_complete: if status == TaskStatus::Completed { Some(100) } else { None },
        reminder_minutes_before: reminder_minutes(value, due),
        categories: string_array(value, "categories"),
        // The Graph id doubles as the write target (an update/delete is
        // `/me/todo/lists/{list}/tasks/{id}`) - the `href` role.
        href: Some(id.to_string()),
        etag: etag(value),
    })
}

/// Graph's absolute `reminderDateTime` as the model's minutes-before-due.
/// `isReminderOn` gates it; a reminder with no due date (or an unparseable
/// one) is dropped rather than given a fabricated anchor.
fn reminder_minutes(value: &Value, due: Option<DateTime<Utc>>) -> Option<i64> {
    if !value.get("isReminderOn").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let due = due?;
    let reminder = value.get("reminderDateTime").and_then(parse_graph_date_time).map(|(instant, _)| instant)?;
    Some((due - reminder).num_minutes())
}

/// The date a task's recurrence is anchored on: its start when one was
/// stored, else its due date (the editor's only visible date).
fn task_anchor_date(task: &CalendarTask) -> Option<NaiveDate> {
    task.start.or(task.due).map(|date| date.date_naive())
}

/// Whether Graph's structured recurrence can represent the task's rule.
/// `true` with no rule; `false` for a rule outside Graph's model (e.g. the
/// `BYSETPOS` first/last-weekday form) or a rule with no date to start from.
pub fn todo_recurrence_supported(task: &CalendarTask) -> bool {
    task.rrule
        .as_deref()
        .is_none_or(|rrule| task_anchor_date(task).and_then(|start| rrule_to_graph(rrule, start, None)).is_some())
}

/// The JSON body to `POST` (create) or `PATCH` (update) a Graph `todoTask`.
/// Only the fields the model carries are written; Graph merges a `PATCH`, so an
/// absent field is untouched (and Graph has no client-writable `percent`).
pub fn task_to_todo_body(task: &CalendarTask) -> Value {
    let mut body = json!({
        "title": task.summary.clone().unwrap_or_default(),
        "importance": importance_to_graph(task.priority),
        "status": status_to_graph(task.status),
    });
    if let Some(description) = task.description.as_deref().filter(|d| !d.is_empty()) {
        body["body"] = json!({ "contentType": "text", "content": description });
    }
    if let Some(due) = task.due {
        body["dueDateTime"] = graph_date_time(due, None);
    }
    if let Some(start) = task.start {
        body["startDateTime"] = graph_date_time(start, None);
    }
    if let Some(rrule) = task.rrule.as_deref() {
        if let Some(recurrence) = task_anchor_date(task).and_then(|start| rrule_to_graph(rrule, start, None)) {
            body["recurrence"] = recurrence;
        }
    }
    match (task.reminder_minutes_before, task.due) {
        (Some(minutes), Some(due)) => {
            body["isReminderOn"] = json!(true);
            body["reminderDateTime"] = graph_date_time(due - chrono::Duration::minutes(minutes), None);
        }
        // Clearing a reminder on a dated task writes the off switch. With no
        // due date the model can't say whether Graph holds a reminder, so the
        // property is left untouched rather than silently cleared.
        (None, Some(_due)) => body["isReminderOn"] = json!(false),
        (Some(_), None) | (None, None) => {}
    }
    if let Some(completed) = task.completed {
        body["completedDateTime"] = graph_date_time(completed, None);
    }
    if !task.categories.is_empty() {
        body["categories"] = json!(task.categories);
    }
    body
}

/// Maps a Graph `checklistItem`, or `None` when it has neither id nor text.
pub fn checklist_item_from_graph(value: &Value) -> Option<ChecklistStep> {
    let id = value.get("id").and_then(Value::as_str).map(str::to_string);
    let display_name = value.get("displayName").and_then(Value::as_str).unwrap_or_default().to_string();
    if id.is_none() && display_name.is_empty() {
        return None;
    }
    Some(ChecklistStep {
        id,
        display_name,
        is_checked: value.get("isChecked").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Maps a Graph `linkedResource`, or `None` when it carries nothing usable.
pub fn linked_resource_from_graph(value: &Value) -> Option<LinkedResource> {
    let link = LinkedResource {
        id: value.get("id").and_then(Value::as_str).map(str::to_string),
        web_url: non_empty_str(value, "webUrl"),
        display_name: non_empty_str(value, "displayName"),
        application_name: non_empty_str(value, "applicationName"),
        external_id: non_empty_str(value, "externalId"),
    };
    if link.id.is_none() && link.web_url.is_none() && link.external_id.is_none() {
        None
    } else {
        Some(link)
    }
}

fn status_from_graph(value: Option<&str>) -> TaskStatus {
    match value {
        Some("completed") => TaskStatus::Completed,
        Some("inProgress") => TaskStatus::InProgress,
        // "notStarted", "waitingOnOthers" and "deferred" all read as
        // "needs action" - the model has no separate waiting state.
        _ => TaskStatus::NeedsAction,
    }
}

fn status_to_graph(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Completed => "completed",
        TaskStatus::InProgress => "inProgress",
        // `Cancelled` has no Graph counterpart; write it back as not-started
        // rather than misrepresenting it as complete.
        TaskStatus::NeedsAction | TaskStatus::Cancelled => "notStarted",
    }
}

/// RFC 5545 `PRIORITY` ⇄ Graph's three-state `importance`. Lossy by nature.
fn priority_from_graph(value: Option<&str>) -> TaskPriority {
    match value {
        Some("high") => TaskPriority(1),
        Some("low") => TaskPriority(9),
        _ => TaskPriority(0),
    }
}

fn importance_to_graph(priority: TaskPriority) -> &'static str {
    match priority.0 {
        0 => "normal",
        1..=4 => "high",
        5 => "normal",
        _ => "low",
    }
}

/// Graph stamps To Do tasks with `@odata.etag` (the write precondition); fall
/// back to `changeKey` for robustness.
fn etag(value: &Value) -> Option<String> {
    non_empty_str(value, "@odata.etag").or_else(|| non_empty_str(value, "changeKey"))
}

/// Graph's `createdDateTime` / `lastModifiedDateTime` carry a trailing `Z`,
/// unlike the `dateTimeTimeZone` fields.
fn parse_iso8601(value: &Value) -> Option<DateTime<Utc>> {
    let raw = value.as_str()?;
    DateTime::parse_from_rfc3339(raw).ok().map(|dt| dt.with_timezone(&Utc))
}

fn body_text(value: &Value) -> Option<String> {
    value
        .get("body")
        .and_then(|body| body.get("content"))
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

fn non_empty_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn string_array(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task() -> Value {
        json!({
            "id": "AAMkAD-task",
            "@odata.etag": "W/\"etag-1\"",
            "title": "Buy milk",
            "body": { "content": "Two litres", "contentType": "text" },
            "importance": "high",
            "status": "inProgress",
            "dueDateTime": { "dateTime": "2026-08-09T09:00:00.0000000", "timeZone": "UTC" },
            "startDateTime": { "dateTime": "2026-08-08T09:00:00.0000000", "timeZone": "UTC" },
            "recurrence": {
                "pattern": { "type": "daily", "interval": 1 },
                "range": { "type": "noEnd", "startDate": "2026-08-08", "recurrenceTimeZone": "UTC" }
            },
            "isReminderOn": true,
            "reminderDateTime": { "dateTime": "2026-08-09T08:30:00.0000000", "timeZone": "UTC" },
            "createdDateTime": "2026-08-01T12:00:00Z",
            "categories": ["Errands", "Home"],
            "checklistItems": [
                { "id": "step-1", "displayName": "Check the fridge", "isChecked": true },
                { "id": "step-2", "displayName": "Walk to shop", "isChecked": false }
            ],
            "linkedResources": [
                { "id": "link-1", "webUrl": "https://outlook.office.com/mail/id/abc", "displayName": "Re: order", "applicationName": "Microsoft Outlook", "externalId": "msg-1" }
            ]
        })
    }

    #[test]
    fn calendar_id_round_trips() {
        let id = graph_todo_calendar_id("list-1");
        assert_eq!(id.0, "todo:list-1");
        assert_eq!(graph_todo_list_id(&id), Some("list-1"));
        assert_eq!(graph_todo_list_id(&CalendarId("googletasks:x".to_string())), None);
    }

    #[test]
    fn list_maps_id_name_and_well_known() {
        let list = todo_list_from_graph(&json!({
            "id": "list-1",
            "displayName": "Groceries",
            "wellknownListName": "defaultList"
        }))
        .unwrap();
        assert_eq!(list.id, "list-1");
        assert_eq!(list.display_name, "Groceries");
        assert_eq!(list.well_known.as_deref(), Some("defaultList"));
        // A nameless list falls back rather than rendering blank.
        assert_eq!(todo_list_from_graph(&json!({ "id": "l2" })).unwrap().display_name, "Tasks");
        assert!(todo_list_from_graph(&json!({ "displayName": "No id" })).is_none());
    }

    #[test]
    fn task_reads_every_mapped_field() {
        let task = todo_task_from_graph(&sample_task(), "list-1").unwrap();
        assert_eq!(task.uid.0, "AAMkAD-task");
        assert_eq!(task.calendar_id.0, "todo:list-1");
        assert_eq!(task.summary.as_deref(), Some("Buy milk"));
        assert_eq!(task.description.as_deref(), Some("Two litres"));
        assert_eq!(task.status, TaskStatus::InProgress);
        assert_eq!(task.priority, TaskPriority(1));
        assert_eq!(task.percent_complete, None);
        assert_eq!(task.due.unwrap().to_rfc3339(), "2026-08-09T09:00:00+00:00");
        assert_eq!(task.start.unwrap().to_rfc3339(), "2026-08-08T09:00:00+00:00");
        assert_eq!(task.created.unwrap().to_rfc3339(), "2026-08-01T12:00:00+00:00");
        assert_eq!(task.categories, vec!["Errands".to_string(), "Home".to_string()]);
        assert_eq!(task.href.as_deref(), Some("AAMkAD-task"));
        assert_eq!(task.etag.as_deref(), Some("W/\"etag-1\""));
    }

    #[test]
    fn completed_task_maps_status_and_percent() {
        let value = json!({ "id": "t", "status": "completed", "completedDateTime": { "dateTime": "2026-08-07T10:00:00", "timeZone": "UTC" } });
        let task = todo_task_from_graph(&value, "l").unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.percent_complete, Some(100));
        assert_eq!(task.completed.unwrap().to_rfc3339(), "2026-08-07T10:00:00+00:00");
    }

    #[test]
    fn a_zone_less_or_malformed_due_drops_gracefully() {
        let value = json!({ "id": "t", "title": "Bad" });
        let task = todo_task_from_graph(&value, "l").unwrap();
        assert_eq!(task.due, None);
        assert_eq!(task.status, TaskStatus::NeedsAction);
        assert!(todo_task_from_graph(&json!({ "title": "no id" }), "l").is_none());
    }

    #[test]
    fn task_write_body_carries_the_model_fields() {
        let task = todo_task_from_graph(&sample_task(), "list-1").unwrap();
        let body = task_to_todo_body(&task);
        assert_eq!(body["title"], "Buy milk");
        assert_eq!(body["body"]["contentType"], "text");
        assert_eq!(body["body"]["content"], "Two litres");
        assert_eq!(body["importance"], "high");
        assert_eq!(body["status"], "inProgress");
        assert_eq!(body["dueDateTime"]["dateTime"], "2026-08-09T09:00:00");
        assert_eq!(body["dueDateTime"]["timeZone"], "UTC");
        assert_eq!(body["categories"], json!(["Errands", "Home"]));
        assert!(body.get("percentComplete").is_none(), "Graph has no percentage");
        assert!(body.get("createdDateTime").is_none(), "createdDateTime is server-set");
    }

    #[test]
    fn recurrence_and_reminder_round_trip_through_the_mapper() {
        let task = todo_task_from_graph(&sample_task(), "list-1").unwrap();
        assert_eq!(task.rrule.as_deref(), Some("FREQ=DAILY"));
        assert_eq!(task.reminder_minutes_before, Some(30));

        let body = task_to_todo_body(&task);
        assert_eq!(body["recurrence"]["pattern"]["type"], "daily");
        assert_eq!(body["recurrence"]["range"]["startDate"], "2026-08-08");
        assert_eq!(body["isReminderOn"], true);
        assert_eq!(body["reminderDateTime"]["dateTime"], "2026-08-09T08:30:00");
        assert_eq!(body["reminderDateTime"]["timeZone"], "UTC");

        // Clearing the reminder writes the off switch.
        let mut cleared = task.clone();
        cleared.reminder_minutes_before = None;
        let body = task_to_todo_body(&cleared);
        assert_eq!(body["isReminderOn"], false);
        assert!(body.get("reminderDateTime").is_none());
    }

    #[test]
    fn only_graph_representable_recurrence_is_supported() {
        let mut task = todo_task_from_graph(&sample_task(), "list-1").unwrap();
        assert!(todo_recurrence_supported(&task));
        task.rrule = Some("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1".to_string());
        assert!(!todo_recurrence_supported(&task));
        task.rrule = None;
        assert!(todo_recurrence_supported(&task));
    }

    #[test]
    fn priority_and_importance_map_both_ways() {
        for (priority, importance) in [(0u8, "normal"), (1, "high"), (4, "high"), (5, "normal"), (9, "low")] {
            assert_eq!(importance_to_graph(TaskPriority(priority)), importance);
        }
        assert_eq!(priority_from_graph(Some("high")), TaskPriority(1));
        assert_eq!(priority_from_graph(Some("normal")), TaskPriority(0));
        assert_eq!(priority_from_graph(Some("low")), TaskPriority(9));
    }

    #[test]
    fn checklist_items_and_linked_resources_map() {
        let value = sample_task();
        let steps: Vec<ChecklistStep> = value
            .get("checklistItems")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(checklist_item_from_graph)
            .collect();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].display_name, "Check the fridge");
        assert!(steps[0].is_checked);

        let link = linked_resource_from_graph(value.get("linkedResources").and_then(Value::as_array).unwrap().first().unwrap()).unwrap();
        assert_eq!(link.external_id.as_deref(), Some("msg-1"));
        assert_eq!(link.application_name.as_deref(), Some("Microsoft Outlook"));
        assert!(linked_resource_from_graph(&json!({})).is_none());
    }

    #[test]
    fn the_captured_fixtures_round_trip_through_the_mapper() {
        let lists_page: Value = serde_json::from_str(include_str!("../../../../test-fixtures/outlook/todo-lists.json")).unwrap();
        let lists: Vec<TodoList> = lists_page.get("value").and_then(Value::as_array).unwrap().iter().filter_map(todo_list_from_graph).collect();
        assert_eq!(lists.len(), 2, "the fixture carries two lists");
        assert_eq!(lists[0].display_name, "Tasks");
        assert_eq!(lists[0].well_known.as_deref(), Some("defaultList"));

        let tasks_page: Value = serde_json::from_str(include_str!("../../../../test-fixtures/outlook/todo-tasks.json")).unwrap();
        let tasks: Vec<CalendarTask> = tasks_page
            .get("value")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|v| todo_task_from_graph(v, "list-1"))
            .collect();
        assert_eq!(tasks.len(), 2, "the fixture carries two tasks");
        assert_eq!(tasks[0].summary.as_deref(), Some("Buy milk"));
        assert_eq!(tasks[0].priority, TaskPriority(1));
        assert_eq!(tasks[0].rrule.as_deref(), Some("FREQ=DAILY"));
        assert_eq!(tasks[0].reminder_minutes_before, Some(30));
        assert_eq!(tasks[1].status, TaskStatus::Completed);
    }
}
