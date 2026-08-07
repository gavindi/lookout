//! Calendar event reminders.
//!
//! Every occurrence the CalDAV sessions report (`OccurrencesUpdated`) that
//! carries a `reminder_minutes_before` (a `VALARM`) is fed into a
//! [`ReminderEngine`] by the window code; a UI-thread tick (every
//! [`ALERT_TICK`]) fires a `Gio.Notification` for each whose alert time has
//! come, whose event hasn't started yet, and which hasn't already been shown,
//! snoozed-past or dismissed. The notification carries Open / Snooze 5 min /
//! Dismiss actions routed through app-level `GSimpleAction`s; fire-once and
//! snooze state persists in the UI-state database
//! (`UiStateDb::load_reminder_state` / `set_reminder_state`), so a restart
//! never re-fires an alert the user already saw. Alerts only fire while the
//! app is running - reminders while Lookout is closed are out of scope.

use std::cell::RefCell;
use std::rc::Rc;

use chrono::Utc;
use gtk::gio;
use gtk::gio::prelude::{ActionMapExt, ApplicationExt};
use gtk::glib;
use lookout_core::{AccountId, EventOccurrence};

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
    /// The occurrence's start in local time, formatted into the
    /// notification body ("Starts at HH:MM").
    pub starts_local: chrono::DateTime<chrono::Local>,
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
            let starts_local = occ.start.with_timezone(&chrono::Local);
            let alert_at = starts_local - chrono::Duration::minutes(minutes);
            // The alert is due once its instant has passed but the event
            // hasn't started yet (a late catch-up after the event began is
            // pointless). `start` is UTC, so `now` is compared on that side.
            if alert_at > now || occ.start <= now.with_timezone(&Utc) {
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

    /// Persists `occ` as fired and updates the in-memory store - the
    /// fire-once step every returned alert goes through.
    fn mark_fired(&mut self, occ: &EventOccurrence) {
        let key = alert_key(occ);
        self.write_state(occ, "fired", None);
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

/// Builds and sends one alert notification: title = the event's summary (or
/// "(No title)"), body = "Starts at HH:MM", default action `app.open-event`,
/// buttons Snooze 5 min (`app.snooze-reminder`) and Dismiss
/// (`app.dismiss-reminder`) - all three carrying the same JSON-serialized
/// occurrence, so every action can rebuild the exact event.
fn show_notification(app: &adw::Application, alert: &DueAlert) {
    let title = alert.occurrence.summary.as_deref().unwrap_or("(No title)");
    let notification = gio::Notification::new(title);
    notification.set_body(Some(&format!("Starts at {}", alert.starts_local.format("%H:%M"))));
    let json = serde_json::to_string(&alert.occurrence).expect("EventOccurrence serialization cannot fail");
    let variant = glib::Variant::from(json);
    notification.set_default_action_and_target_value("app.open-event", Some(&variant));
    notification.add_button_with_target_value("Snooze 5 min", "app.snooze-reminder", Some(&variant));
    notification.add_button_with_target_value("Dismiss", "app.dismiss-reminder", Some(&variant));
    app.send_notification(Some(&notification_id(&alert.occurrence)), &notification);
}

/// Wires the reminder loop into the application:
///
/// * Registers the `open-event`, `snooze-reminder` and `dismiss-reminder`
///   actions on `app`. All three take a string target holding the
///   JSON-serialized [`EventOccurrence`]. `open-event` hands it to
///   `open_event` (the window code's editor-opening closure);
///   `snooze-reminder`/`dismiss-reminder` update the engine's persisted
///   state and withdraw the notification.
/// * Runs one tick immediately (startup catch-up: an alert that came due
///   while the app was closed still appears, as long as its event hasn't
///   started), then a repeating [`ALERT_TICK`] timer.
/// * Each tick sends a notification per due alert when
///   `settings.get_bool(crate::settings::CALENDAR_ALERTS_ENABLED)` is true:
///   title = the event's summary (or "(No title)"), body = "Starts at HH:MM",
///   default action `app.open-event`, buttons Snooze 5 min (`app.snooze-reminder`)
///   and Dismiss (`app.dismiss-reminder`), sent under `notification_id`.
pub fn spawn_reminder_loop(
    app: &adw::Application,
    engine: Rc<RefCell<ReminderEngine>>,
    settings: Rc<crate::settings::SettingsStore>,
    open_event: Rc<dyn Fn(EventOccurrence) + 'static>,
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

    // One startup tick (so an alert that came due while the app was closed
    // still appears if its event hasn't started), then the repeating timer.
    // While alerts are disabled, the tick is not even consulted, so nothing
    // gets marked fired while off - re-enabling fires what is still due.
    let app = app.clone();
    let run_tick = move || {
        if settings.get_bool(crate::settings::CALENDAR_ALERTS_ENABLED) {
            let due = engine.borrow_mut().tick(chrono::Local::now());
            for alert in due {
                show_notification(&app, &alert);
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
            rrule: None,
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: Default::default(),
            transparency: Default::default(),
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
        assert_eq!(due[0].starts_local, start.with_timezone(&chrono::Local));
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
}
