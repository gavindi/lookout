//! Calendar event and task reminders.
//!
//! Every occurrence the CalDAV sessions report (`OccurrencesUpdated`) that
//! carries a `reminder_minutes_before` (a `VALARM`) is fed into a
//! [`ReminderEngine`] by the window code; the Tasks view's merged snapshot
//! feeds task reminders the same way (`sync_tasks`, T-05). A UI-thread tick
//! (every [`ALERT_TICK`]) fires a `Gio.Notification` for each whose alert time
//! has come, which hasn't already been shown, snoozed-past or dismissed. The
//! notification carries Open / Snooze 5 min / Dismiss actions routed through
//! app-level `GSimpleAction`s; fire-once and snooze state persists in the
//! UI-state database (`UiStateDb::load_reminder_state` / `set_reminder_state`),
//! so a restart never re-fires an alert the user already saw. Alerts only fire
//! while the app is running - reminders while Lookout is closed are out of
//! scope.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use chrono::{TimeZone, Utc};
use gtk::gio;
use gtk::gio::prelude::{ActionMapExt, ApplicationExt};
use gtk::glib;
use lookout_core::{AccountId, CalendarTask, EventOccurrence};

/// How often the loop re-checks for due alerts. 30 s granularity means an
/// alert can land up to 30 s late, which is fine for a desktop reminder.
pub const ALERT_TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// How far out the Snooze action re-arms an alert, in minutes.
pub const SNOOZE_MINUTES: i64 = 5;

/// Occurrences whose start is further in the past than this are pruned from
/// the engine's index (and their persisted rows cleared), so the in-memory
/// index and the database don't grow without bound.
const PRUNE_WINDOW: chrono::Duration = chrono::Duration::days(2);

/// The stable identity of one occurrence's alert:
/// `calendar_id|uid|start-utc`. This is the primary key of the persisted
/// reminder state and the basis of the notification id, so the same
/// occurrence always maps to the same key no matter how many times it is
/// re-synced.
pub fn alert_key(occ: &EventOccurrence) -> String {
    format!("{}|{}|{}", occ.calendar_id.0, occ.uid.0, occ.start.format("%Y-%m-%dT%H:%M:%SZ"))
}

/// The notification id passed to `Application::send_notification` /
/// `withdraw_notification` for one occurrence's alert. Distinct per
/// occurrence; replacing a notification happens by sending again under the
/// same id (a snoozed re-fire is a new notification under the same id).
pub fn notification_id(occ: &EventOccurrence) -> String {
    alert_key(occ)
}

/// One alert ready to be shown by the tick.
#[derive(Debug, Clone)]
pub struct DueAlert {
    pub occurrence: EventOccurrence,
    /// The occurrence's start as wall-clock time in the calendar's display
    /// zone (item 3.1), formatted into the notification body ("Starts at
    /// HH:MM").
    pub starts_local: chrono::NaiveDateTime,
}

/// One task alert ready to be shown by the tick (T-05).
#[derive(Debug, Clone)]
pub struct DueTaskAlert {
    pub task: CalendarTask,
    /// The due time as wall-clock time in the display zone.
    pub due_local: chrono::NaiveDateTime,
    /// Whether the due time has already passed when the alert fires - the
    /// notification then says "Overdue" (the task view still shows it).
    pub overdue: bool,
}

/// The stable identity of one task's alert: `calendar|task:<uid>|due-utc`.
/// The `task:` prefix keeps it disjoint from an event alert's key and is
/// exactly the uid stored in the `reminder_state` table, so the persisted
/// fire-once state maps back without a schema change.
pub fn task_alert_key(task: &CalendarTask) -> String {
    let due = task.due.map(|due| due.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default();
    format!("{}|task:{}|{}", task.calendar_id.0, task.uid.0, due)
}

/// The fire-once/alert bookkeeping for every known occurrence.
///
/// Owned by the UI thread (GTK is single-threaded) like the rest of the
/// app's `Rc<RefCell<_>>` state. `tick` is pure apart from the database
/// writes it makes to mark alerts as shown, so its logic is unit-testable
/// without any GTK object.
pub struct ReminderEngine {
    // NOTE: the fields are private to this module; keep it that way.
    // - `index`: every occurrence currently watched, keyed by `alert_key`,
    //   with the account it came from.
    // - `store`: the persisted state rows, loaded once from the UI-state db.
    //   Values are `("fired"|"snoozed", Option<snooze_until_utc>)`.
    // - `ui_db`: the database handle, kept for state writes; a failed open
    //   means "no persistence" - alerts still fire, they just re-fire after
    //   a restart.
    index: std::collections::HashMap<String, (AccountId, EventOccurrence)>,
    /// Every task currently carrying a reminder, keyed by [`task_alert_key`].
    /// Replaced wholesale by [`ReminderEngine::sync_tasks`] on each Tasks-view
    /// refresh, so completed/deleted tasks disappear without stale alerts.
    task_index: std::collections::HashMap<String, (AccountId, CalendarTask)>,
    store: std::collections::HashMap<String, (String, Option<String>)>,
    ui_db: Option<Rc<RefCell<crate::ui_state_db::UiStateDb>>>,
}

impl ReminderEngine {
    /// Opens the persisted state from `store` (best-effort: a `None` handle
    /// or an unreadable database just means no fire-once memory this
    /// session - alerts still fire, they re-fire after a restart).
    pub fn new(store: Option<Rc<RefCell<crate::ui_state_db::UiStateDb>>>) -> Self {
        let mut loaded = std::collections::HashMap::new();
        if let Some(db) = &store {
            match db.borrow().load_reminder_state() {
                Ok(rows) => {
                    for row in rows {
                        let key = format!("{}|{}|{}", row.calendar_id, row.uid, row.start_utc);
                        loaded.insert(key, (row.state, row.snooze_until_utc));
                    }
                }
                Err(e) => tracing::warn!("could not load reminder state: {e}"),
            }
        }
        ReminderEngine {
            index: std::collections::HashMap::new(),
            task_index: std::collections::HashMap::new(),
            store: loaded,
            ui_db: store,
        }
    }

    /// Feeds one account's freshly-synced occurrences into the index,
    /// replacing any earlier occurrence with the same key (a resync of the
    /// same month reports the same occurrences; a change to the event
    /// updates in place). Called from the `OccurrencesUpdated` handler, so
    /// the engine sees every month the sessions fetch, never just the
    /// displayed one.
    pub fn ingest(&mut self, account_id: &AccountId, occurrences: &[EventOccurrence]) {
        for occ in occurrences {
            self.index.insert(alert_key(occ), (account_id.clone(), occ.clone()));
        }
    }

    /// Replaces the task alerts with the Tasks view's current merged snapshot
    /// (`(account, task)` pairs from every CalDAV, Graph, Google and local
    /// source). Only tasks with both a due time and a reminder are indexed; a
    /// task that disappears or is completed simply stops being tracked, while
    /// its persisted fire-once row stays until the pruning window passes.
    pub fn sync_tasks(&mut self, tasks: &[(AccountId, CalendarTask)]) {
        self.task_index.clear();
        for (account_id, task) in tasks {
            if task.due.is_none() || task.reminder_minutes_before.is_none() {
                continue;
            }
            self.task_index.insert(task_alert_key(task), (account_id.clone(), task.clone()));
        }
    }

    /// The task alerts to show at `now` (T-05): a reminder is due once
    /// `due - reminder_minutes_before` has passed. Unlike an event alert this
    /// keeps firing for an overdue task (once per occurrence) so a reminder
    /// missed while the app was closed still surfaces; the notification then
    /// says "Overdue". Persisted `fired`/`snoozed` state is honoured exactly as
    /// for events, and each returned alert is marked fired immediately.
    pub fn tick_tasks(&mut self, now: chrono::DateTime<chrono::Local>) -> Vec<DueTaskAlert> {
        // Drop persisted rows for task occurrences more than a month old;
        // their key embeds the due stamp, so the index needn't still hold the
        // task to clear it.
        let cutoff = (now - chrono::Duration::days(30)).with_timezone(&Utc);
        let stale: Vec<String> = self
            .store
            .keys()
            .filter(|key| key.contains("|task:"))
            .filter(|key| {
                key.rsplit('|')
                    .next()
                    .and_then(|stamp| chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H:%M:%SZ").ok())
                    .is_some_and(|due| Utc.from_utc_datetime(&due) < cutoff)
            })
            .cloned()
            .collect();
        for key in stale {
            if let Some((_, task)) = self.task_index.get(&key) {
                self.clear_task_row(task);
            }
            self.store.remove(&key);
        }

        let now_utc = now.with_timezone(&Utc);
        let zone = crate::calendar_tz::display_zone();
        let mut due = Vec::new();
        for (key, (_, task)) in &self.task_index {
            let (Some(minutes), Some(due_at)) = (task.reminder_minutes_before, task.due) else {
                continue;
            };
            let alert_at = due_at - chrono::Duration::minutes(minutes);
            if alert_at > now_utc {
                continue;
            }
            if let Some((state, snooze_until)) = self.store.get(key) {
                if state == "fired" {
                    continue;
                }
                if state == "snoozed" {
                    if let Some(until) = snooze_until {
                        if let Ok(until) = chrono::DateTime::parse_from_rfc3339(until) {
                            if until.with_timezone(&Utc) > now_utc {
                                continue;
                            }
                        }
                    }
                }
            }
            due.push(DueTaskAlert {
                task: task.clone(),
                due_local: zone.occurrence_to_local(due_at, false),
                overdue: due_at <= now_utc,
            });
        }
        for alert in &due {
            self.mark_task_fired(&alert.task);
        }
        due
    }

    /// The alerts to show at `now`: occurrences with a `reminder_minutes_before`,
    /// whose alert time (`start - reminder`, in local time) has passed, whose
    /// event hasn't started yet, and whose persisted state is neither `fired`
    /// nor a `snoozed` entry still in the future. Also prunes stale
    /// occurrences. Every returned alert is immediately marked `fired` in the
    /// store, so a later tick can't re-return it.
    pub fn tick(&mut self, now: chrono::DateTime<chrono::Local>) -> Vec<DueAlert> {
        // Drop occurrences that started more than PRUNE_WINDOW ago: their
        // alert can never legitimately fire again, and clearing their rows
        // (plus the persisted ones) keeps the index and the database bounded.
        let prune_before = (now - PRUNE_WINDOW).with_timezone(&Utc);
        let stale: Vec<String> = self.index.iter().filter(|(_, (_, occ))| occ.start < prune_before).map(|(key, _)| key.clone()).collect();
        for key in stale {
            if let Some((_, occ)) = self.index.remove(&key) {
                self.clear_db_row(&occ);
                self.store.remove(&key);
            }
        }

        let mut due = Vec::new();
        for (key, (_, occ)) in &self.index {
            let Some(minutes) = occ.reminder_minutes_before else { continue };
            // An all-day event starts at local midnight of its date (its
            // stored start is that date's UTC midnight, see `calendar_tz`).
            let zone = crate::calendar_tz::display_zone();
            let starts_at = zone.occurrence_instant(occ.start, occ.all_day);
            let starts_local = zone.occurrence_to_local(occ.start, occ.all_day);
            let alert_at = starts_at - chrono::Duration::minutes(minutes);
            // The alert is due once its instant has passed but the event
            // hasn't started yet (a late catch-up after the event began is
            // pointless). Both sides compare as UTC instants.
            if alert_at > now.with_timezone(&Utc) || starts_at <= now.with_timezone(&Utc) {
                continue;
            }
            // Persisted state wins over the calendar: a fired alert never
            // re-fires, and a snoozed one is quiet until its deadline passes
            // (an unparseable deadline is treated as not-snoozed, so a stale
            // row can't dead-letter the alert).
            if let Some((state, snooze_until)) = self.store.get(key) {
                if state == "fired" {
                    continue;
                }
                if state == "snoozed" {
                    if let Some(until) = snooze_until {
                        if let Ok(until) = chrono::DateTime::parse_from_rfc3339(until) {
                            if until.with_timezone(&Utc) > now.with_timezone(&Utc) {
                                continue;
                            }
                        }
                    }
                }
            }
            due.push(DueAlert {
                occurrence: occ.clone(),
                starts_local,
            });
        }
        // Mark everything we are about to hand out as fired immediately, so
        // a later tick (or a re-entrant one) can never re-return it.
        for alert in &due {
            self.mark_fired(&alert.occurrence);
        }
        due
    }

    /// Marks the occurrence's alert snoozed until `now + SNOOZE_MINUTES`;
    /// the caller withdraws the notification. The next tick whose `now`
    /// passes the snooze point re-returns it (as long as the event still
    /// hasn't started).
    pub fn snooze(&mut self, occ: &EventOccurrence) {
        let until = (chrono::Local::now() + chrono::Duration::minutes(SNOOZE_MINUTES)).to_utc().to_rfc3339();
        let key = alert_key(occ);
        self.write_state(occ, "snoozed", Some(&until));
        self.store.insert(key, ("snoozed".to_string(), Some(until)));
    }

    /// Marks the occurrence's alert permanently fired (a user dismissal);
    /// the caller withdraws the notification. Never returned by `tick` again.
    pub fn dismiss(&mut self, occ: &EventOccurrence) {
        let key = alert_key(occ);
        self.write_state(occ, "fired", None);
        self.store.insert(key, ("fired".to_string(), None));
    }

    /// Snoozes a task alert until `now + SNOOZE_MINUTES`, the task-side twin
    /// of [`Self::snooze`].
    pub fn snooze_task(&mut self, task: &CalendarTask) {
        let until = (chrono::Local::now() + chrono::Duration::minutes(SNOOZE_MINUTES)).to_utc().to_rfc3339();
        let key = task_alert_key(task);
        self.write_task_state(task, "snoozed", Some(&until));
        self.store.insert(key, ("snoozed".to_string(), Some(until)));
    }

    /// Permanently dismisses a task alert, the task-side twin of
    /// [`Self::dismiss`].
    pub fn dismiss_task(&mut self, task: &CalendarTask) {
        let key = task_alert_key(task);
        self.write_task_state(task, "fired", None);
        self.store.insert(key, ("fired".to_string(), None));
    }

    /// Drops every occurrence ingested from `account_id` (a disabled GOA
    /// account): their alerts must not fire, and their index/database rows
    /// are removed so a re-enable starts clean.
    pub fn remove_account(&mut self, account_id: &AccountId) {
        let removed: Vec<String> = self.index.iter().filter(|(_, (account, _))| account == account_id).map(|(key, _)| key.clone()).collect();
        for key in removed {
            if let Some((_, occ)) = self.index.remove(&key) {
                self.clear_db_row(&occ);
                self.store.remove(&key);
            }
        }
        let removed_tasks: Vec<String> = self
            .task_index
            .iter()
            .filter(|(_, (account, _))| account == account_id)
            .map(|(key, _)| key.clone())
            .collect();
        for key in removed_tasks {
            if let Some((_, task)) = self.task_index.remove(&key) {
                self.clear_task_row(&task);
                self.store.remove(&key);
            }
        }
    }

    /// Best-effort write of `(state, snooze_until)` for `occ` to the
    /// UI-state db, mirroring the store's primary key exactly. A failed
    /// write only means the state won't survive a restart.
    fn write_state(&self, occ: &EventOccurrence, state: &str, snooze_until_utc: Option<&str>) {
        let start_utc = occ.start.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        if let Some(db) = &self.ui_db {
            if let Err(e) = db.borrow().set_reminder_state(&occ.calendar_id.0, &occ.uid.0, &start_utc, state, snooze_until_utc) {
                tracing::warn!(key = alert_key(occ), "could not persist reminder state: {e}");
            }
        }
    }

    /// Best-effort delete of `occ`'s persisted row from the UI-state db,
    /// used when the occurrence is pruned. A failed delete leaves a stale
    /// row behind, but a pruned occurrence can never be ingested again.
    fn clear_db_row(&self, occ: &EventOccurrence) {
        let start_utc = occ.start.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        if let Some(db) = &self.ui_db {
            if let Err(e) = db.borrow().clear_reminder(&occ.calendar_id.0, &occ.uid.0, &start_utc) {
                tracing::warn!(key = alert_key(occ), "could not clear reminder state: {e}");
            }
        }
    }

    /// The task twin of [`Self::write_state`]: the `reminder_state` table's
    /// `uid` column carries the `task:` prefix so a task and an event can
    /// never share a key.
    fn write_task_state(&self, task: &CalendarTask, state: &str, snooze_until_utc: Option<&str>) {
        let Some(due) = task.due else { return };
        let due_utc = due.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let uid = format!("task:{}", task.uid.0);
        if let Some(db) = &self.ui_db {
            if let Err(e) = db.borrow().set_reminder_state(&task.calendar_id.0, &uid, &due_utc, state, snooze_until_utc) {
                tracing::warn!(key = task_alert_key(task), "could not persist task reminder state: {e}");
            }
        }
    }

    /// Best-effort delete of a task's persisted reminder row.
    fn clear_task_row(&self, task: &CalendarTask) {
        let Some(due) = task.due else { return };
        let due_utc = due.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let uid = format!("task:{}", task.uid.0);
        if let Some(db) = &self.ui_db {
            if let Err(e) = db.borrow().clear_reminder(&task.calendar_id.0, &uid, &due_utc) {
                tracing::warn!(key = task_alert_key(task), "could not clear task reminder state: {e}");
            }
        }
    }

    /// Persists `occ` as fired and updates the in-memory store - the
    /// fire-once step every returned alert goes through.
    fn mark_fired(&mut self, occ: &EventOccurrence) {
        let key = alert_key(occ);
        self.write_state(occ, "fired", None);
        self.store.insert(key, ("fired".to_string(), None));
    }

    /// The task twin of [`Self::mark_fired`].
    fn mark_task_fired(&mut self, task: &CalendarTask) {
        let key = task_alert_key(task);
        self.write_task_state(task, "fired", None);
        self.store.insert(key, ("fired".to_string(), None));
    }
}

/// Parses the string target of one of the reminder actions (the
/// JSON-serialized [`EventOccurrence`]) back into an occurrence. A missing
/// or unparseable target is logged and yields `None` - the action is then a
/// no-op, which is right for an externally-crafted activation.
fn parse_action_target(param: Option<&glib::Variant>) -> Option<EventOccurrence> {
    let json = param.and_then(|v| v.get::<String>())?;
    match serde_json::from_str::<EventOccurrence>(&json) {
        Ok(occ) => Some(occ),
        Err(e) => {
            tracing::warn!("reminder action target is not a serialized EventOccurrence: {e}");
            None
        }
    }
}

/// Parses the string target of one of the task-reminder actions (the
/// JSON-serialized [`CalendarTask`]). A missing or unparseable target is a
/// no-op, matching [`parse_action_target`].
fn parse_task_action_target(param: Option<&glib::Variant>) -> Option<CalendarTask> {
    let json = param.and_then(|v| v.get::<String>())?;
    match serde_json::from_str::<CalendarTask>(&json) {
        Ok(task) => Some(task),
        Err(e) => {
            tracing::warn!("task reminder action target is not a serialized CalendarTask: {e}");
            None
        }
    }
}

/// Builds and sends one alert notification: title = the event's summary (or
/// "(No title)"), body = "Starts at HH:MM", default action `app.open-event`,
/// buttons Snooze 5 min (`app.snooze-reminder`) and Dismiss
/// (`app.dismiss-reminder`) - all three carrying the same JSON-serialized
/// occurrence, so every action can rebuild the exact event.
fn show_notification(app: &adw::Application, alert: &DueAlert) {
    let title = alert.occurrence.summary.as_deref().unwrap_or("(No title)");
    // All-day events (birthdays among them) anchor at local midnight, so
    // "Starts at 00:00" would be noise - "All day" reads better.
    let body = if alert.occurrence.all_day {
        "All day".to_string()
    } else {
        format!("Starts at {}", alert.starts_local.format("%H:%M"))
    };
    let notification = gio::Notification::new(title);
    notification.set_body(Some(&body));
    let json = serde_json::to_string(&alert.occurrence).expect("EventOccurrence serialization cannot fail");
    let variant = glib::Variant::from(json);
    notification.set_default_action_and_target_value("app.open-event", Some(&variant));
    // CAL-20 / 3.17: a Join button for an online meeting, opening the detected
    // join URL in the default browser.
    if let Some(meeting) = lookout_core::interop::onlinemeeting::detect(
        alert.occurrence.conference_url.as_deref(),
        alert.occurrence.location.as_deref(),
        alert.occurrence.description.as_deref(),
    ) {
        let url_variant = glib::Variant::from(meeting.url);
        notification.add_button_with_target_value("Join", "app.join-meeting", Some(&url_variant));
    }
    notification.add_button_with_target_value("Snooze 5 min", "app.snooze-reminder", Some(&variant));
    notification.add_button_with_target_value("Dismiss", "app.dismiss-reminder", Some(&variant));
    app.send_notification(Some(&notification_id(&alert.occurrence)), &notification);
}

/// Builds and sends one task reminder (T-05): title = the task's summary (or
/// "(untitled)"), body = "Due at HH:MM" (or "Overdue · due …"), default action
/// `app.open-task`, buttons Snooze 5 min and Dismiss. The notification id is
/// the task alert key, so a re-fire under the same occurrence replaces rather
/// than duplicates it.
fn show_task_notification(app: &adw::Application, alert: &DueTaskAlert) {
    let title = alert.task.summary.as_deref().unwrap_or("(untitled)");
    let due = alert.due_local;
    let body = if alert.overdue {
        format!("Overdue \u{b7} due {}", due.format("%a %d %b, %H:%M"))
    } else if due.date() == crate::calendar_tz::display_zone().today() {
        format!("Due today at {}", due.format("%H:%M"))
    } else {
        format!("Due {} at {}", due.format("%a %d %b"), due.format("%H:%M"))
    };
    let notification = gio::Notification::new(title);
    notification.set_body(Some(&body));
    let json = serde_json::to_string(&alert.task).expect("CalendarTask serialization cannot fail");
    let variant = glib::Variant::from(json);
    notification.set_default_action_and_target_value("app.open-task", Some(&variant));
    notification.add_button_with_target_value("Snooze 5 min", "app.snooze-task-reminder", Some(&variant));
    notification.add_button_with_target_value("Dismiss", "app.dismiss-task-reminder", Some(&variant));
    app.send_notification(Some(&task_alert_key(&alert.task)), &notification);
}

/// Wires the reminder loop into the application:
///
/// * Registers the `open-event`, `snooze-reminder` and `dismiss-reminder`
///   actions on `app` (and the task twins `open-task`,
///   `snooze-task-reminder`, `dismiss-task-reminder`). All take a string target
///   holding the JSON-serialized [`EventOccurrence`]/[`CalendarTask`].
///   `open-event`/`open-task` hand it to the window's editor-opening closures;
///   the snooze/dismiss actions update the engine's persisted state and
///   withdraw the notification.
/// * Runs one tick immediately (startup catch-up: an alert that came due
///   while the app was closed still appears), then a repeating [`ALERT_TICK`]
///   timer, covering both events and tasks.
/// * Each tick sends a notification per due alert when
///   `settings.get_bool(crate::settings::CALENDAR_ALERTS_ENABLED)` is true.
pub fn spawn_reminder_loop(
    app: &adw::Application,
    engine: Rc<RefCell<ReminderEngine>>,
    settings: Rc<crate::settings::SettingsStore>,
    open_event: Rc<dyn Fn(EventOccurrence) + 'static>,
    open_task: Rc<dyn Fn(CalendarTask) + 'static>,
) {
    let open_action = gio::SimpleAction::new("open-event", Some(glib::VariantTy::STRING));
    open_action.connect_activate({
        let open_event = open_event.clone();
        move |_, param| {
            let Some(occ) = parse_action_target(param) else { return };
            open_event(occ);
        }
    });
    app.add_action(&open_action);

    let open_task_action = gio::SimpleAction::new("open-task", Some(glib::VariantTy::STRING));
    open_task_action.connect_activate({
        let open_task = open_task.clone();
        move |_, param| {
            let Some(task) = parse_task_action_target(param) else { return };
            open_task(task);
        }
    });
    app.add_action(&open_task_action);

    let snooze_action = gio::SimpleAction::new("snooze-reminder", Some(glib::VariantTy::STRING));
    snooze_action.connect_activate({
        let engine = engine.clone();
        let app = app.clone();
        move |_, param| {
            let Some(occ) = parse_action_target(param) else { return };
            engine.borrow_mut().snooze(&occ);
            app.withdraw_notification(&notification_id(&occ));
        }
    });
    app.add_action(&snooze_action);

    let dismiss_action = gio::SimpleAction::new("dismiss-reminder", Some(glib::VariantTy::STRING));
    dismiss_action.connect_activate({
        let engine = engine.clone();
        let app = app.clone();
        move |_, param| {
            let Some(occ) = parse_action_target(param) else { return };
            engine.borrow_mut().dismiss(&occ);
            app.withdraw_notification(&notification_id(&occ));
        }
    });
    app.add_action(&dismiss_action);

    let snooze_task_action = gio::SimpleAction::new("snooze-task-reminder", Some(glib::VariantTy::STRING));
    snooze_task_action.connect_activate({
        let engine = engine.clone();
        let app = app.clone();
        move |_, param| {
            let Some(task) = parse_task_action_target(param) else { return };
            engine.borrow_mut().snooze_task(&task);
            app.withdraw_notification(&task_alert_key(&task));
        }
    });
    app.add_action(&snooze_task_action);

    let dismiss_task_action = gio::SimpleAction::new("dismiss-task-reminder", Some(glib::VariantTy::STRING));
    dismiss_task_action.connect_activate({
        let engine = engine.clone();
        let app = app.clone();
        move |_, param| {
            let Some(task) = parse_task_action_target(param) else { return };
            engine.borrow_mut().dismiss_task(&task);
            app.withdraw_notification(&task_alert_key(&task));
        }
    });
    app.add_action(&dismiss_task_action);

    // CAL-20 / 3.17: opens an online-meeting join URL (the notification's
    // Join button) in the default browser.
    let join_action = gio::SimpleAction::new("join-meeting", Some(glib::VariantTy::STRING));
    join_action.connect_activate(move |_, param| {
        if let Some(url) = param.and_then(|value| value.str()) {
            crate::window::open_uri_in_default_browser(url);
        }
    });
    app.add_action(&join_action);

    // One startup tick (so an alert that came due while the app was closed
    // still appears if its event hasn't started), then the repeating timer.
    // While alerts are disabled, the tick is not even consulted, so nothing
    // gets marked fired while off - re-enabling fires what is still due.
    let app = app.clone();
    let run_tick = move || {
        if settings.get_bool(crate::settings::CALENDAR_ALERTS_ENABLED) {
            let now = chrono::Local::now();
            let due = engine.borrow_mut().tick(now);
            for alert in due {
                show_notification(&app, &alert);
            }
            let due_tasks = engine.borrow_mut().tick_tasks(now);
            for alert in due_tasks {
                show_task_notification(&app, &alert);
            }
        }
        glib::ControlFlow::Continue
    };
    run_tick();
    glib::timeout_add_local(ALERT_TICK, run_tick);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_state_db::UiStateDb;
    use chrono::{DateTime, Duration, Utc};
    use lookout_core::{CalendarId, EventUid};

    // A helper to build a minimal occurrence (big struct, many fields):
    // shared by several tests. The engine's UiStateDb is isolated in a temp
    // dir (see `engine_with` below for the env-var convention).
    #[track_caller]
    fn occ(uid: &str, start: &str, minutes: Option<i64>) -> EventOccurrence {
        let start = DateTime::parse_from_rfc3339(start).unwrap().with_timezone(&Utc);
        EventOccurrence {
            uid: EventUid(uid.into()),
            calendar_id: CalendarId("cal-test".into()),
            summary: Some("Test".into()),
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
            attendees: Vec::new(),
            organizer: None,
            schedule_agent: None,
            categories: Vec::new(),
            sensitivity: Default::default(),
            show_as: Default::default(),
            status: Default::default(),
            reminder_minutes_before: minutes,
            conference_url: None,
        }
    }

    // A fresh engine over a real UiStateDb, pre-ingested with `occurrences`.
    // The db handle is returned too, so tests can check what the engine
    // persisted.
    //
    // `UiStateDb::open()` derives its path from the process-global
    // `XDG_CACHE_HOME`, which `ui_state_db`'s tests also set - so the
    // set-env-then-open window is serialized across both modules by the
    // shared `ui_state_db::CACHE_HOME_LOCK` (each test still gets its own
    // pid-scoped subdirectory).
    #[track_caller]
    fn engine_with(test_name: &str, occurrences: &[EventOccurrence]) -> (ReminderEngine, Rc<RefCell<UiStateDb>>) {
        let dir = std::env::temp_dir().join(format!("lookout-reminders-test-{}", std::process::id())).join(test_name);
        let _guard = crate::ui_state_db::CACHE_HOME_LOCK.lock().unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let db = Rc::new(RefCell::new(UiStateDb::open().expect("fresh database should open")));
        drop(_guard);
        let mut engine = ReminderEngine::new(Some(db.clone()));
        engine.ingest(&AccountId("test-account".into()), occurrences);
        (engine, db)
    }

    // The persisted row for exactly `event`'s key, if the engine wrote one.
    fn persisted_row(db: &UiStateDb, occ: &EventOccurrence) -> Option<crate::ui_state_db::ReminderStateRow> {
        let key = alert_key(occ);
        db.load_reminder_state()
            .expect("test database should read")
            .into_iter()
            .find(|r| format!("{}|{}|{}", r.calendar_id, r.uid, r.start_utc) == key)
    }

    #[test]
    fn alert_key_is_stable_and_distinct() {
        let a = occ("uid-1", "2026-08-07T09:00:00Z", Some(10));
        // The key is calendar_id|uid|start only: re-syncing the same
        // occurrence (even with different reminder settings) maps to the
        // same key, and the notification id rides on it.
        let resynced = occ("uid-1", "2026-08-07T09:00:00Z", None);
        assert_eq!(alert_key(&a), alert_key(&resynced));
        assert_eq!(notification_id(&a), alert_key(&a));
        // A different start or a different uid is a different alert.
        assert_ne!(alert_key(&a), alert_key(&occ("uid-1", "2026-08-07T10:00:00Z", Some(10))));
        assert_ne!(alert_key(&a), alert_key(&occ("uid-2", "2026-08-07T09:00:00Z", Some(10))));
    }

    #[test]
    fn tick_fires_due_alert_once() {
        let now = chrono::Local::now();
        // Event starts 30 min out, alert 15 min before: alert_at = now + 15.
        let start = now + Duration::minutes(30);
        let event = occ("fires-once", &start.with_timezone(&Utc).to_rfc3339(), Some(15));
        let (mut engine, db) = engine_with("fires-once", &[event]);

        // Before the alert instant: nothing.
        assert!(engine.tick(now + Duration::minutes(5)).is_empty());
        // Once it passes: exactly one alert, with the start in local time.
        let due = engine.tick(now + Duration::minutes(16));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].occurrence.uid.0, "fires-once");
        assert_eq!(due[0].starts_local, start.naive_local());
        // Fired once means never again.
        assert!(engine.tick(now + Duration::minutes(25)).is_empty());
        // And the fire is persisted as a "fired" row with no deadline.
        let row = persisted_row(&db.borrow(), &due[0].occurrence).expect("the fired alert must be persisted");
        assert_eq!(row.state, "fired");
        assert_eq!(row.snooze_until_utc, None);
    }

    #[test]
    fn tick_skips_unfired_future_alerts() {
        let now = chrono::Local::now();
        // Event starts in an hour, alert 15 min before: alert_at = now + 45.
        let start = now + Duration::hours(1);
        let event = occ("future", &start.with_timezone(&Utc).to_rfc3339(), Some(15));
        let (mut engine, db) = engine_with("future", std::slice::from_ref(&event));

        // Neither now nor 40 min out (still before the alert instant) fires.
        assert!(engine.tick(now).is_empty());
        assert!(engine.tick(now + Duration::minutes(40)).is_empty());
        // Skipped ticks persist nothing for this event.
        assert!(persisted_row(&db.borrow(), &event).is_none());
    }

    #[test]
    fn tick_skips_alerts_for_started_events() {
        let now = chrono::Local::now();
        // The reminder was 15 min before a start that has already passed:
        // the alert instant is long gone, but so is the event.
        let start = now - Duration::minutes(10);
        let event = occ("started", &start.with_timezone(&Utc).to_rfc3339(), Some(15));
        let (mut engine, db) = engine_with("started", std::slice::from_ref(&event));

        assert!(engine.tick(now).is_empty());
        assert!(persisted_row(&db.borrow(), &event).is_none());
    }

    #[test]
    fn snooze_refires_after_the_snooze_window() {
        let now = chrono::Local::now();
        // Event starts in 10 min with a 10-min reminder: alert_at = now, so
        // it fires at now + 1 min while the event is still upcoming.
        let start = now + Duration::minutes(10);
        let event = occ("snoozed", &start.with_timezone(&Utc).to_rfc3339(), Some(10));
        let (mut engine, db) = engine_with("snooze", &[event]);

        let due = engine.tick(now + Duration::minutes(1));
        assert_eq!(due.len(), 1);

        // Snoozing replaces the row with "snoozed" plus a deadline about
        // SNOOZE_MINUTES out (wall-clock based, so assert the window, not
        // the exact instant).
        engine.snooze(&due[0].occurrence);
        let row = persisted_row(&db.borrow(), &due[0].occurrence).expect("the snoozed alert must be persisted");
        assert_eq!(row.state, "snoozed");
        let snooze_until = row.snooze_until_utc.as_deref().expect("snoozed row carries a deadline").parse::<DateTime<Utc>>().unwrap();
        assert!(snooze_until > now.with_timezone(&Utc));
        assert!(snooze_until < (now + Duration::minutes(6)).with_timezone(&Utc));

        // While the snooze is live the alert stays quiet even though it is
        // due; after the deadline passes it re-fires (its event still hasn't
        // started), and the re-fire is persisted like a fresh one.
        assert!(engine.tick(now + Duration::minutes(2)).is_empty());
        let due = engine.tick(now + Duration::minutes(7));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].occurrence.uid.0, "snoozed");
        let row = persisted_row(&db.borrow(), &due[0].occurrence).expect("the re-fired alert must be persisted");
        assert_eq!(row.state, "fired");
    }

    #[test]
    fn dismiss_never_refires() {
        let now = chrono::Local::now();
        let start = now + Duration::minutes(30);
        let event = occ("dismissed", &start.with_timezone(&Utc).to_rfc3339(), Some(15));
        let (mut engine, db) = engine_with("dismiss", &[event]);

        let due = engine.tick(now + Duration::minutes(16));
        assert_eq!(due.len(), 1);
        engine.dismiss(&due[0].occurrence);

        // A dismissal is permanent, however far past the alert instant.
        assert!(engine.tick(now + Duration::minutes(20)).is_empty());
        assert!(engine.tick(now + Duration::minutes(25)).is_empty());
        let row = persisted_row(&db.borrow(), &due[0].occurrence).expect("the dismissed alert must stay persisted");
        assert_eq!(row.state, "fired");
        assert_eq!(row.snooze_until_utc, None);
    }

    #[test]
    fn prune_drops_stale_occurrences_and_their_rows() {
        let now = chrono::Local::now();
        // The stale occurrence started 3 days ago (past the 2-day window);
        // the fresh one is still upcoming.
        let stale = occ("stale", &(now - Duration::days(3)).with_timezone(&Utc).to_rfc3339(), Some(15));
        let fresh = occ("fresh", &(now + Duration::minutes(30)).with_timezone(&Utc).to_rfc3339(), Some(15));
        let (mut engine, db) = engine_with("prune", &[stale.clone(), fresh.clone()]);
        // A previous session had fired the stale alert; its row must not
        // survive the prune.
        let stale_start = stale.start.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        db.borrow().set_reminder_state("cal-test", "stale", &stale_start, "fired", None).unwrap();

        assert!(engine.tick(now).is_empty());
        // The stale occurrence is gone from the index and the store; the
        // fresh one is untouched (and not due yet).
        assert_eq!(engine.index.len(), 1);
        assert!(engine.index.contains_key(&alert_key(&fresh)));
        assert!(!engine.index.contains_key(&alert_key(&stale)));
        assert!(!engine.store.contains_key(&alert_key(&stale)));
        // And its persisted row is cleared from the database.
        assert!(persisted_row(&db.borrow(), &stale).is_none());
        assert!(persisted_row(&db.borrow(), &fresh).is_none());
    }

    #[test]
    fn remove_account_prunes_its_alerts_and_persisted_rows() {
        let now = chrono::Local::now();
        let event = occ("removed", &(now + Duration::minutes(30)).with_timezone(&Utc).to_rfc3339(), Some(15));
        let other = occ("other", &(now + Duration::minutes(30)).with_timezone(&Utc).to_rfc3339(), Some(15));
        let (mut engine, db) = engine_with("remove-account", std::slice::from_ref(&event));
        engine.ingest(&AccountId("other-account".into()), std::slice::from_ref(&other));
        // A previous session had fired the disabled account's alert; its
        // persisted row must not survive the teardown.
        let event_start = event.start.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        db.borrow().set_reminder_state("cal-test", "removed", &event_start, "fired", None).unwrap();

        engine.remove_account(&AccountId("test-account".into()));

        // The disabled account's alert is gone - index, store, and database.
        assert_eq!(engine.index.len(), 1);
        assert!(engine.index.contains_key(&alert_key(&other)));
        assert!(!engine.index.contains_key(&alert_key(&event)));
        assert!(!engine.store.contains_key(&alert_key(&event)));
        assert!(persisted_row(&db.borrow(), &event).is_none());
        // The other account's alert still fires normally; the removed one
        // never does.
        let due = engine.tick(now + Duration::minutes(16));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].occurrence.uid.0, "other");
        assert!(engine.tick(now + Duration::minutes(20)).is_empty());
    }

    fn task(uid: &str, due: &str, minutes: Option<i64>) -> CalendarTask {
        CalendarTask {
            uid: lookout_core::TaskUid(uid.into()),
            calendar_id: CalendarId("cal-test".into()),
            summary: Some("Task".into()),
            description: None,
            due: Some(DateTime::parse_from_rfc3339(due).unwrap().with_timezone(&Utc)),
            start: None,
            rrule: None,
            created: None,
            completed: None,
            status: lookout_core::TaskStatus::NeedsAction,
            priority: lookout_core::TaskPriority(0),
            percent_complete: None,
            reminder_minutes_before: minutes,
            categories: Vec::new(),
            href: None,
            etag: None,
        }
    }

    fn persisted_task_row(db: &UiStateDb, task: &CalendarTask) -> Option<crate::ui_state_db::ReminderStateRow> {
        let key = task_alert_key(task);
        db.load_reminder_state()
            .expect("test database should read")
            .into_iter()
            .find(|r| format!("{}|{}|{}", r.calendar_id, r.uid, r.start_utc) == key)
    }

    #[test]
    fn tick_tasks_fires_a_due_reminder_once_and_persists_it() {
        let now = chrono::Local::now();
        let start = now + Duration::minutes(30);
        let task = task("task-fires-once", &start.with_timezone(&Utc).to_rfc3339(), Some(15));
        let (mut engine, db) = engine_with("task-fires-once", &[]);
        engine.sync_tasks(&[(AccountId("test-account".into()), task.clone())]);

        // Before the alert instant: nothing.
        assert!(engine.tick_tasks(now + Duration::minutes(5)).is_empty());
        // Once it passes: one alert, not overdue yet.
        let due = engine.tick_tasks(now + Duration::minutes(16));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].task.uid.0, "task-fires-once");
        assert!(!due[0].overdue);
        // Fired once means never again, and the row is persisted.
        assert!(engine.tick_tasks(now + Duration::minutes(25)).is_empty());
        let row = persisted_task_row(&db.borrow(), &task).expect("the fired task alert must be persisted");
        assert_eq!(row.state, "fired");
        assert!(row.uid.starts_with("task:"));
    }

    #[test]
    fn an_overdue_task_reminder_still_fires_with_the_overdue_flag() {
        let now = chrono::Local::now();
        // Due 10 minutes ago with a 15-minute reminder: the alert is overdue
        // but must still surface when the app starts.
        let due_at = now - Duration::minutes(10);
        let task = task("task-overdue", &due_at.with_timezone(&Utc).to_rfc3339(), Some(15));
        let (mut engine, _db) = engine_with("task-overdue", &[]);
        engine.sync_tasks(&[(AccountId("test-account".into()), task)]);
        let due = engine.tick_tasks(now);
        assert_eq!(due.len(), 1);
        assert!(due[0].overdue);
    }

    #[test]
    fn sync_tasks_replaces_the_snapshot_and_skips_unreminded_tasks() {
        let now = chrono::Local::now();
        let start = now + Duration::minutes(30);
        let reminded = task("kept", &start.with_timezone(&Utc).to_rfc3339(), Some(10));
        let unreminded = task("ignored", &start.with_timezone(&Utc).to_rfc3339(), None);
        let (mut engine, _db) = engine_with("task-sync", &[]);
        engine.sync_tasks(&[(AccountId("a".into()), reminded.clone()), (AccountId("a".into()), unreminded)]);
        assert_eq!(engine.task_index.len(), 1);

        // A later snapshot without the task drops its index entry (the
        // persisted row is left for the age-based prune).
        engine.sync_tasks(&[]);
        assert!(engine.task_index.is_empty());
        assert!(engine.tick_tasks(now + Duration::minutes(21)).is_empty());
    }

    #[test]
    fn snoozed_task_reminders_refire_after_the_window() {
        let now = chrono::Local::now();
        let start = now + Duration::minutes(10);
        let task = task("task-snoozed", &start.with_timezone(&Utc).to_rfc3339(), Some(10));
        let (mut engine, db) = engine_with("task-snooze", &[]);
        engine.sync_tasks(&[(AccountId("a".into()), task.clone())]);

        let due = engine.tick_tasks(now + Duration::minutes(1));
        assert_eq!(due.len(), 1);
        engine.snooze_task(&due[0].task);
        let row = persisted_task_row(&db.borrow(), &task).expect("the snoozed task alert must be persisted");
        assert_eq!(row.state, "snoozed");

        assert!(engine.tick_tasks(now + Duration::minutes(2)).is_empty());
        let due = engine.tick_tasks(now + Duration::minutes(7));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].task.uid.0, "task-snoozed");
        assert!(persisted_task_row(&db.borrow(), &task).is_some_and(|row| row.state == "fired"));
    }
}
