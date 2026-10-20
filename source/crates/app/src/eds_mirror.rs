//! Turning one [`EventOccurrence`] into the event that goes into Lookout's
//! EDS mirror calendar (EDS_TODO.md, E2.2).
//!
//! The mirror stores expanded occurrences, not recurring series: each instance
//! becomes a plain, non-repeating `VEVENT` with its own stable UID, so GNOME
//! Shell's clock and GNOME Calendar see concrete events without Lookout having
//! to reproduce recurrence rules, exceptions and time-zone details. A later
//! instance of a series is a different mirror event, and editing an occurrence
//! changes its copy in place instead of creating a second one.
//!
//! Only what the Shell needs is copied: the summary, the start and end, the
//! all-day flag, the time zone and the location. Descriptions are a privacy
//! choice (`include_description`, wired to the setting in E2.4): a description
//! routinely holds dial-in codes, agendas and notes, and EDS data is readable
//! by every app on the desktop. Attendees, reminders, categories and the
//! booking's "Show as" are deliberately not copied - the mirror is a one-way
//! calendar view, not a second copy of Lookout's data.
//!
//! [`mirror_event`] is pure. E2.3's sync reads a rolling window of cached
//! occurrences, turns them into [`MirrorEvent`]s, and rewrites the mirror's
//! whole iCalendar file through [`build_calendar`] - the custom-file design,
//! so EDS sees one file change and reloads it. The per-event
//! [`MirrorEvent::hash`] values decide whether anything actually changed;
//! [`plan`] reports the creates, modifies and removes for the log, while the
//! file itself is written atomically only when that plan is non-empty (or the
//! file is missing).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, Datelike, Months, NaiveDate, Utc};
use gtk::glib;
use lookout_core::{AccountId, CalendarEvent, CalendarId, EventOccurrence, EventStatus, EventUid, RecurrenceRange};
use sha2::{Digest, Sha256};

use crate::window::CalendarUiState;
use crate::worker::Worker;

/// How far either side of now the mirror reaches, in days (E2.3).
const WINDOW_PAST_DAYS: i64 = 7;
const WINDOW_FUTURE_DAYS: i64 = 60;

/// The 5 s trailing-edge debounce the calendar event loops use after a month
/// that overlaps the mirror window is written (E2.3).
pub(crate) const SYNC_DEBOUNCE: Duration = Duration::from_secs(5);

/// How often a running Lookout re-syncs on its own, so the window keeps
/// moving forward even on a machine that never refreshes a calendar (E2.3).
const SYNC_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The prefix of every mirror UID, so the planner can recognise its own events
/// among anything else the calendar might report.
const MIRROR_UID_PREFIX: &str = "lookout-";

/// The `X-` property holding [`MirrorEvent::hash`] in the mirrored `VEVENT`.
/// EDS preserves `X-` properties across its store, and E2.3 reads this back to
/// tell which copies changed.
const HASH_PROPERTY: &str = "X-LOOKOUT-HASH";

/// One event as it appears in the mirror calendar.
#[allow(dead_code)] // E2.3 is the first non-test caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MirrorEvent {
    /// The stable UID of the mirror copy: `lookout-<hash>` over the source
    /// calendar, the source UID and the occurrence's start. Re-reading the
    /// same occurrence (or editing it) yields the same UID; a different
    /// instance of a series yields a different one.
    pub(crate) uid: String,
    /// A hash of the fields copied into the mirror (see [`mirror_event`]),
    /// mirrored as [`HASH_PROPERTY`]. E2.3 skips events whose hash matches.
    pub(crate) hash: String,
    /// The occurrence's start, kept for the sync window's bookkeeping.
    pub(crate) start: DateTime<Utc>,
    /// The occurrence's end, kept for the sync window's bookkeeping.
    pub(crate) end: DateTime<Utc>,
    /// The `VEVENT` - plus the `VTIMEZONE` it needs, if any - ready to sit
    /// inside the mirror's `VCALENDAR`. Always non-repeating.
    pub(crate) component: String,
}

/// Converts one occurrence into a mirror event, or `None` when it must not be
/// mirrored: it was cancelled, or its calendar is hidden in Lookout.
///
/// `include_description` copies the event's notes as well; see the module
/// comment for why that is opt-in.
#[allow(dead_code)] // E2.3 is the first non-test caller.
pub(crate) fn mirror_event(occurrence: &EventOccurrence, include_description: bool, hidden_calendars: &HashSet<CalendarId>) -> Option<MirrorEvent> {
    if occurrence.status == EventStatus::Cancelled || hidden_calendars.contains(&occurrence.calendar_id) {
        return None;
    }
    // An all-day event's `VALUE=DATE` boundaries are zone-independent, so the
    // serializer ignores any stored zone. Drop it from the hash too, or a zone
    // the copy doesn't use would look like a change on every sync.
    let time_zone = if occurrence.all_day { None } else { occurrence.time_zone.clone() };
    let hash = copy_hash(occurrence, time_zone.as_deref(), include_description);
    let uid = mirror_uid(occurrence);

    let event = stripped_event(occurrence, time_zone, include_description, &uid);
    let component = mirror_component(&lookout_dav::build_vcalendar(&event), &hash)?;

    Some(MirrorEvent {
        uid,
        hash,
        start: occurrence.start,
        end: occurrence.end,
        component,
    })
}

/// The stable mirror UID. `lookout-` plus the first 128 bits of a SHA-256 over
/// the source calendar, source UID and occurrence start, which is unique to
/// one instance of a series and survives a re-read.
fn mirror_uid(occurrence: &EventOccurrence) -> String {
    let mut hasher = Sha256::new();
    hasher.update(occurrence.calendar_id.0.as_bytes());
    hasher.update([0x1f]);
    hasher.update(occurrence.uid.0.as_bytes());
    hasher.update([0x1f]);
    hasher.update(occurrence.start.timestamp_micros().to_be_bytes());
    format!("{MIRROR_UID_PREFIX}{}", hex(&hasher.finalize()[..16]))
}

/// A hash of exactly the fields [`mirror_event`] copies, so a change to
/// anything left out (attendees, categories, a reminder, an href) doesn't make
/// E2.3 rewrite the mirror. Field names and lengths are part of the canonical
/// text, so no two different field values can hash the same.
fn copy_hash(occurrence: &EventOccurrence, time_zone: Option<&str>, include_description: bool) -> String {
    let start = occurrence.start.timestamp_micros().to_string();
    let end = occurrence.end.timestamp_micros().to_string();
    let mut canonical = String::new();
    field(&mut canonical, "summary", occurrence.summary.as_deref());
    field(&mut canonical, "start", Some(&start));
    field(&mut canonical, "end", Some(&end));
    field(&mut canonical, "all_day", Some(if occurrence.all_day { "1" } else { "0" }));
    field(&mut canonical, "time_zone", time_zone);
    field(&mut canonical, "location", occurrence.location.as_deref());
    field(&mut canonical, "description", if include_description { occurrence.description.as_deref() } else { None });
    hex(&Sha256::digest(canonical.as_bytes()))
}

/// Appends one canonical field. A missing value is distinct from an empty one,
/// so adding a summary or clearing it always changes the hash.
fn field(out: &mut String, name: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            out.push_str(name);
            out.push('=');
            out.push_str(&value.len().to_string());
            out.push(':');
            out.push_str(value);
        }
        None => {
            out.push_str(name);
            out.push_str("=-");
        }
    }
    out.push('\n');
}

/// The stripped-down [`CalendarEvent`] `build_vcalendar` serializes: the
/// copied fields only, no recurrence of any kind and no participants.
fn stripped_event(occurrence: &EventOccurrence, time_zone: Option<String>, include_description: bool, mirror_uid: &str) -> CalendarEvent {
    CalendarEvent {
        uid: EventUid(mirror_uid.to_string()),
        calendar_id: occurrence.calendar_id.clone(),
        summary: occurrence.summary.clone(),
        description: if include_description { occurrence.description.clone() } else { None },
        location: occurrence.location.clone(),
        start: occurrence.start,
        end: occurrence.end,
        all_day: occurrence.all_day,
        time_zone,
        rrule: None,
        recurrence_id: None,
        recurrence_range: RecurrenceRange::default(),
        exdates: Vec::new(),
        rdates: Vec::new(),
        href: None,
        etag: None,
        sequence: 0,
        attendees: Vec::new(),
        organizer: None,
        schedule_agent: None,
        categories: Vec::new(),
        sensitivity: Default::default(),
        show_as: Default::default(),
        status: EventStatus::Confirmed,
        reminder_minutes_before: None,
        conference_url: None,
        is_online_meeting: false,
    }
}

/// Cuts the `VCALENDAR` wrapper off `build_vcalendar`'s output, keeping the
/// `VTIMEZONE` (when there is one) and the `VEVENT`, and stamps the copy hash
/// into the event.
fn mirror_component(ics: &str, hash: &str) -> Option<String> {
    let start = ics.find("BEGIN:VTIMEZONE").or_else(|| ics.find("BEGIN:VEVENT"))?;
    let vevent_end = ics.find("END:VEVENT")?;
    let end = ics[vevent_end..].find('\n').map_or(ics.len(), |offset| vevent_end + offset + 1);
    let mut component = ics.get(start..end)?.to_string();
    let insert_at = component.rfind("END:VEVENT")?;
    component.insert_str(insert_at, &format!("{HASH_PROPERTY}:{hash}\r\n"));
    Some(component)
}

/// Lower-case hex, so hashes are stable and safe as iCalendar property values.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// What one E2.3 sync needs to know: which caches to read and which
/// calendars to leave out. Built on the GTK thread from the calendar state,
/// then moved to the worker.
#[derive(Debug, Clone)]
pub(crate) struct MirrorConfig {
    /// Calendar-account ids and webcal subscriptions, as the cache keys them
    /// (`webcal-<subscription id>` for a feed).
    pub(crate) sources: Vec<AccountId>,
    pub(crate) hidden_calendars: HashSet<CalendarId>,
    pub(crate) include_description: bool,
}

/// What a sync did, for the debug log and for tests.
#[derive(Debug)]
pub(crate) struct MirrorOutcome {
    /// How many events the desired calendar contains.
    pub(crate) events: usize,
    /// Whether the file was rewritten. `false` means the comparison found
    /// nothing to do.
    pub(crate) written: bool,
    pub(crate) plan: MirrorPlan,
}

/// The per-UID difference between the desired mirror and what the file
/// currently holds. The custom-file design doesn't apply these individually -
/// it rewrites the whole calendar - but they say what changed, and their
/// emptiness is what skips the rewrite.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MirrorPlan {
    pub(crate) create: Vec<String>,
    pub(crate) modify: Vec<String>,
    pub(crate) remove: Vec<String>,
}

impl MirrorPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.create.is_empty() && self.modify.is_empty() && self.remove.is_empty()
    }
}

/// Compares the desired events against the `(uid, hash)` pairs read back from
/// the mirror file. Pure, so every case E2.3 lists - an empty mirror, an
/// unchanged one, a moved event, a deleted event, one that left the window -
/// is easy to pin.
pub(crate) fn plan(desired: &[MirrorEvent], existing: &[(String, String)]) -> MirrorPlan {
    let desired_by_uid: HashMap<&str, &str> = desired.iter().map(|event| (event.uid.as_str(), event.hash.as_str())).collect();
    let existing_by_uid: HashMap<&str, &str> = existing.iter().map(|(uid, hash)| (uid.as_str(), hash.as_str())).collect();
    let mut plan = MirrorPlan::default();
    for event in desired {
        match existing_by_uid.get(event.uid.as_str()) {
            None => plan.create.push(event.uid.clone()),
            Some(hash) if *hash != event.hash => plan.modify.push(event.uid.clone()),
            Some(_) => {}
        }
    }
    for uid in existing_by_uid.keys() {
        if !desired_by_uid.contains_key(uid) {
            plan.remove.push((*uid).to_string());
        }
    }
    plan.create.sort();
    plan.modify.sort();
    plan.remove.sort();
    plan
}

/// The mirror file's location under `$XDG_DATA_HOME` (or `~/.local/share`),
/// the same host-readable path E0.1's custom-file source needs. E2.4 passes
/// this to `ensure_mirror_source`.
pub(crate) fn mirror_path() -> PathBuf {
    glib::user_data_dir().join("lookout").join("eds-mirror.ics")
}

/// The first-of-month cache keys the rolling window can touch. The sessions
/// bucket occurrences by the UTC date of their start, so this is the set of
/// month caches worth reading.
pub(crate) fn mirror_months(now: DateTime<Utc>) -> Vec<NaiveDate> {
    let first = first_of_month((now - chrono::Duration::days(WINDOW_PAST_DAYS)).date_naive());
    let last = first_of_month((now + chrono::Duration::days(WINDOW_FUTURE_DAYS)).date_naive());
    let mut months = Vec::new();
    let mut month = first;
    while month <= last {
        months.push(month);
        month = month + Months::new(1);
    }
    months
}

/// Whether a synced month could change the mirror, used to avoid scheduling
/// a sync for a month entirely outside the window.
pub(crate) fn month_overlaps_window(month: NaiveDate, now: DateTime<Utc>) -> bool {
    let start = (now - chrono::Duration::days(WINDOW_PAST_DAYS)).date_naive();
    let end = (now + chrono::Duration::days(WINDOW_FUTURE_DAYS)).date_naive();
    let month_start = first_of_month(month);
    let next_month = month_start + Months::new(1);
    month_start <= end && next_month > start
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

/// Turns a batch of cached occurrences into deduplicated mirror events inside
/// the window. An event that only overlaps the window (a multi-day event
/// starting before it) is kept; zero-length events need only their start
/// inside it.
pub(crate) fn mirror_events_from(occurrences: &[EventOccurrence], now: DateTime<Utc>, include_description: bool, hidden_calendars: &HashSet<CalendarId>) -> Vec<MirrorEvent> {
    let window_start = now - chrono::Duration::days(WINDOW_PAST_DAYS);
    let window_end = now + chrono::Duration::days(WINDOW_FUTURE_DAYS);
    let mut by_uid: HashMap<String, MirrorEvent> = HashMap::new();
    for occurrence in occurrences {
        let overlaps = occurrence.start < window_end && (occurrence.end > window_start || (occurrence.end == occurrence.start && occurrence.start >= window_start));
        if !overlaps {
            continue;
        }
        if let Some(event) = mirror_event(occurrence, include_description, hidden_calendars) {
            by_uid.insert(event.uid.clone(), event);
        }
    }
    let mut events: Vec<MirrorEvent> = by_uid.into_values().collect();
    events.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.uid.cmp(&b.uid)));
    events
}

/// Builds the whole mirror `VCALENDAR` from the desired events. The events
/// are sorted by start (then UID) and their `VTIMEZONE`s are deduplicated by
/// `TZID`, so an unchanged calendar has a stable shape even though each
/// component's `DTSTAMP` is generated when it is built.
pub(crate) fn build_calendar(events: &[MirrorEvent]) -> String {
    let mut sorted: Vec<&MirrorEvent> = events.iter().collect();
    sorted.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.uid.cmp(&b.uid)));

    let mut components = String::new();
    let mut timezones: BTreeMap<String, String> = BTreeMap::new();
    for event in sorted {
        let (timezone, vevent) = split_component(&event.component);
        if let Some(timezone) = timezone {
            if let Some(tzid) = timezone_id(timezone) {
                timezones.entry(tzid.to_string()).or_insert_with(|| timezone.to_string());
            }
        }
        components.push_str(vevent);
    }

    let mut calendar = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Lookout//EDS mirror//EN\r\nCALSCALE:GREGORIAN\r\n");
    for timezone in timezones.values() {
        calendar.push_str(timezone);
    }
    calendar.push_str(&components);
    calendar.push_str("END:VCALENDAR\r\n");
    calendar
}

/// `(VTIMEZONE prefix, VEVENT)` from one [`MirrorEvent::component`], or
/// `(None, whole component)` for a zone-free event.
fn split_component(component: &str) -> (Option<&str>, &str) {
    match component.find("BEGIN:VEVENT") {
        Some(index) if index > 0 => (Some(&component[..index]), &component[index..]),
        _ => (None, component),
    }
}

fn timezone_id(timezone: &str) -> Option<&str> {
    timezone
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("TZID:").map(str::trim).filter(|id| !id.is_empty()))
}

/// Reads the UID and `X-LOOKOUT-HASH` of every VEVENT in an existing mirror
/// file. Only our own properties matter here; anything without a UID is
/// ignored, and a missing hash reads as an empty one (so a hand-edited event
/// is treated as changed).
pub(crate) fn existing_events(ics: &str) -> Vec<(String, String)> {
    let mut events = Vec::new();
    let mut in_event = false;
    let mut uid: Option<String> = None;
    let mut hash: Option<String> = None;
    for line in ics.lines() {
        let line = line.trim_end_matches('\r');
        if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
            in_event = true;
            uid = None;
            hash = None;
        } else if line.eq_ignore_ascii_case("END:VEVENT") {
            if let Some(uid) = uid.take() {
                events.push((uid, hash.take().unwrap_or_default()));
            }
            in_event = false;
        } else if in_event {
            if let Some(value) = line.strip_prefix("UID:") {
                uid = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("X-LOOKOUT-HASH:") {
                hash = Some(value.trim().to_string());
            }
        }
    }
    events
}

/// Runs one E2.3 sync off the GTK thread: read the window from every source's
/// `CalendarCache`, compare with the file on disk, and rewrite it atomically
/// when anything changed.
pub(crate) async fn sync_mirror(config: MirrorConfig, path: PathBuf) -> Result<MirrorOutcome, String> {
    tokio::task::spawn_blocking(move || sync_mirror_blocking(&config, &path))
        .await
        .map_err(|e| format!("mirror sync task failed: {e}"))?
}

fn sync_mirror_blocking(config: &MirrorConfig, path: &Path) -> Result<MirrorOutcome, String> {
    let now = Utc::now();
    let months = mirror_months(now);
    let mut sources = config.sources.clone();
    sources.sort();
    sources.dedup();

    let mut occurrences = Vec::new();
    for source in &sources {
        let cache = match lookout_dav::CalendarCache::open(source) {
            Ok(cache) => cache,
            Err(e) => {
                tracing::warn!("EDS mirror: couldn't open {source}'s calendar cache: {e}");
                continue;
            }
        };
        for month in &months {
            match cache.load_month(*month) {
                Ok(Some(month_events)) => occurrences.extend(month_events),
                Ok(None) => {}
                Err(e) => tracing::warn!("EDS mirror: couldn't read {source}'s month {month}: {e}"),
            }
        }
    }

    let desired = mirror_events_from(&occurrences, now, config.include_description, &config.hidden_calendars);
    let existing_text = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("couldn't read {}: {e}", path.display())),
    };
    let existing = existing_text.as_deref().map(existing_events).unwrap_or_default();
    let plan = plan(&desired, &existing);
    let has_calendar = existing_text.as_deref().is_some_and(|text| !text.trim().is_empty());
    if plan.is_empty() && has_calendar {
        return Ok(MirrorOutcome {
            events: desired.len(),
            written: false,
            plan,
        });
    }

    let calendar = build_calendar(&desired);
    write_atomic(path, &calendar).map_err(|e| format!("couldn't write {}: {e}", path.display()))?;
    tracing::info!(
        "EDS mirror: wrote {} events ({} new, {} changed, {} removed)",
        desired.len(),
        plan.create.len(),
        plan.modify.len(),
        plan.remove.len()
    );
    Ok(MirrorOutcome {
        events: desired.len(),
        written: true,
        plan,
    })
}

/// Writes through a pid-suffixed temp file and renames it over the mirror, so
/// EDS's file monitor never sees a half-written calendar (E0.1's atomic-rename
/// reload path).
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// The pacing state stored on `CalendarUiState`. E2.4 fills `enabled` and
/// `include_description` from the settings; until then both default off, so
/// the triggers already wired into the calendar loops do nothing.
#[derive(Default)]
pub(crate) struct MirrorSyncState {
    pub(crate) enabled: bool,
    pub(crate) include_description: bool,
    running: bool,
    pending: bool,
    debounce: Option<glib::SourceId>,
}

impl MirrorSyncState {
    /// Starts a run now, or records that another one is wanted after the
    /// current one (the `GraphPropertySyncs` coalescing rule).
    fn begin(&mut self) -> bool {
        if !self.enabled {
            return false;
        }
        if self.running {
            self.pending = true;
            return false;
        }
        self.running = true;
        true
    }

    /// Ends a run and reports whether a later request arrived meanwhile.
    fn finish(&mut self) -> bool {
        self.running = false;
        let pending = self.pending && self.enabled;
        self.pending = false;
        pending
    }
}

/// E2.4 will call this with the `eds-mirror-calendar` and
/// `eds-mirror-details` settings. Turning it on syncs at once, so the startup
/// trigger isn't the only way the first calendar appears.
#[allow(dead_code)] // E2.4 adds the setting and its switch.
pub(crate) fn set_enabled(calendar_state: &Rc<RefCell<CalendarUiState>>, worker: &Rc<Worker>, enabled: bool, include_description: bool) {
    let changed = {
        let mut state = calendar_state.borrow_mut();
        let changed = state.eds_mirror.enabled != enabled || state.eds_mirror.include_description != include_description;
        state.eds_mirror.enabled = enabled;
        state.eds_mirror.include_description = include_description;
        if !enabled {
            if let Some(timer) = state.eds_mirror.debounce.take() {
                timer.remove();
            }
            state.eds_mirror.pending = false;
        }
        changed
    };
    if enabled && changed {
        request_sync(calendar_state, worker, None);
    }
}

/// Asks for a sync after `delay` (the calendar loops use
/// [`SYNC_DEBOUNCE`]); `None` starts it now. Requests that arrive while one
/// is running coalesce into a single follow-up.
pub(crate) fn request_sync(calendar_state: &Rc<RefCell<CalendarUiState>>, worker: &Rc<Worker>, delay: Option<Duration>) {
    {
        let mut state = calendar_state.borrow_mut();
        if !state.eds_mirror.enabled {
            return;
        }
        if let Some(timer) = state.eds_mirror.debounce.take() {
            timer.remove();
        }
    }
    match delay {
        Some(delay) => {
            let timer_state = calendar_state.clone();
            let timer_worker = worker.clone();
            let timer = glib::timeout_add_local_once(delay, move || {
                timer_state.borrow_mut().eds_mirror.debounce = None;
                start_sync(&timer_state, &timer_worker);
            });
            calendar_state.borrow_mut().eds_mirror.debounce = Some(timer);
        }
        None => start_sync(calendar_state, worker),
    }
}

fn start_sync(calendar_state: &Rc<RefCell<CalendarUiState>>, worker: &Rc<Worker>) {
    let config = {
        let mut state = calendar_state.borrow_mut();
        if !state.eds_mirror.begin() {
            return;
        }
        mirror_config(&state)
    };
    let path = mirror_path();
    let (result_tx, result_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let result = sync_mirror(config, path).await;
        let _ = result_tx.send(result).await;
    });

    let calendar_state = calendar_state.clone();
    let worker = worker.clone();
    glib::spawn_future_local(async move {
        match result_rx.recv().await {
            Ok(Ok(outcome)) => tracing::debug!(
                "EDS mirror: {} events, written={} ({} new, {} changed, {} removed)",
                outcome.events,
                outcome.written,
                outcome.plan.create.len(),
                outcome.plan.modify.len(),
                outcome.plan.remove.len()
            ),
            Ok(Err(e)) => tracing::warn!("EDS mirror sync failed: {e}"),
            Err(_) => tracing::warn!("EDS mirror sync worker stopped before reporting"),
        }
        let follow_up = calendar_state.borrow_mut().eds_mirror.finish();
        if follow_up {
            start_sync(&calendar_state, &worker);
        }
    });
}

fn mirror_config(state: &CalendarUiState) -> MirrorConfig {
    let mut sources: Vec<AccountId> = state.accounts.keys().cloned().collect();
    sources.extend(state.webcal_subscriptions.iter().map(|subscription| AccountId(format!("webcal-{}", subscription.id))));
    sources.sort();
    sources.dedup();
    MirrorConfig {
        sources,
        hidden_calendars: state.hidden_calendars.iter().cloned().collect(),
        include_description: state.eds_mirror.include_description,
    }
}

/// Starts the once-a-day request, so the 7-back/60-ahead window keeps moving
/// even when no calendar is refreshed. A disabled mirror makes it a no-op.
pub(crate) fn start_daily_sync(calendar_state: Rc<RefCell<CalendarUiState>>, worker: Rc<Worker>) {
    glib::timeout_add_seconds_local(SYNC_INTERVAL.as_secs() as u32, move || {
        request_sync(&calendar_state, &worker, None);
        glib::ControlFlow::Continue
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use lookout_core::{AccountId, Attendee, AttendeeRole, AttendeeStatus, BusyStatus, CalendarId, EmailAddress, EventSensitivity, EventUid};

    fn at(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).unwrap()
    }

    fn occurrence() -> EventOccurrence {
        EventOccurrence {
            uid: EventUid("evt-1@example.com".to_string()),
            calendar_id: CalendarId("acct:cal".to_string()),
            summary: Some("Review".to_string()),
            description: Some("Secret notes".to_string()),
            location: Some("Room 1".to_string()),
            start: at(2026, 7, 15, 14),
            end: at(2026, 7, 15, 15),
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
            sensitivity: EventSensitivity::default(),
            show_as: BusyStatus::default(),
            status: EventStatus::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    fn hidden() -> HashSet<CalendarId> {
        HashSet::new()
    }

    fn build(occ: &EventOccurrence) -> MirrorEvent {
        mirror_event(occ, false, &hidden()).expect("an ordinary occurrence is mirrored")
    }

    #[test]
    fn uid_is_stable_and_unique_to_one_instance() {
        let occ = occurrence();
        let first = build(&occ);
        assert_eq!(first.uid, build(&occ.clone()).uid, "re-reading an event keeps its UID");
        assert!(first.uid.starts_with("lookout-"), "mirror UIDs are recognisable: {}", first.uid);

        let mut moved = occ.clone();
        moved.start += Duration::days(1);
        moved.end += Duration::days(1);
        assert_ne!(first.uid, build(&moved).uid, "another instance is another event");

        let mut other_calendar = occ.clone();
        other_calendar.calendar_id = CalendarId("acct:other".to_string());
        assert_ne!(first.uid, build(&other_calendar).uid, "the same UID in another calendar is another event");

        let mut other_event = occ.clone();
        other_event.uid = EventUid("evt-2@example.com".to_string());
        assert_ne!(first.uid, build(&other_event).uid);
    }

    #[test]
    fn all_day_events_are_written_as_dates() {
        let mut occ = occurrence();
        occ.all_day = true;
        occ.start = at(2026, 7, 20, 0);
        occ.end = at(2026, 7, 21, 0);
        occ.time_zone = Some("Europe/Berlin".to_string());
        let event = build(&occ);
        assert!(event.component.starts_with("BEGIN:VEVENT"), "{}", event.component);
        assert!(event.component.contains("DTSTART;VALUE=DATE:20260720"), "{}", event.component);
        assert!(event.component.contains("DTEND;VALUE=DATE:20260721"), "{}", event.component);
        assert!(!event.component.contains("TZID"), "an all-day event is zone-free: {}", event.component);
        assert!(!event.component.contains("VTIMEZONE"), "{}", event.component);
    }

    #[test]
    fn zoned_events_keep_their_zone() {
        let mut occ = occurrence();
        occ.time_zone = Some("Europe/Berlin".to_string());
        let event = build(&occ);
        assert!(event.component.starts_with("BEGIN:VTIMEZONE"), "{}", event.component);
        assert!(event.component.contains("DTSTART;TZID=Europe/Berlin:20260715T160000"), "{}", event.component);
        assert!(event.component.contains("DTEND;TZID=Europe/Berlin:20260715T170000"), "{}", event.component);
    }

    #[test]
    fn recurrence_is_flattened_away() {
        let mut occ = occurrence();
        occ.rrule = Some("FREQ=WEEKLY;COUNT=10".to_string());
        occ.recurrence_id = Some(occ.start);
        occ.exdates = vec![occ.start + Duration::days(7)];
        let event = build(&occ);
        assert!(!event.component.contains("RRULE"), "{}", event.component);
        assert!(!event.component.contains("EXDATE"), "{}", event.component);
        assert!(!event.component.contains("RECURRENCE-ID"), "{}", event.component);
    }

    #[test]
    fn the_component_is_a_bare_vevent_with_the_hash() {
        let event = build(&occurrence());
        assert!(event.component.starts_with("BEGIN:VEVENT"), "{}", event.component);
        assert!(event.component.ends_with("END:VEVENT\r\n"), "{}", event.component);
        assert!(!event.component.contains("BEGIN:VCALENDAR"), "{}", event.component);
        assert!(event.component.contains(&format!("UID:{}\r\n", event.uid)), "{}", event.component);
        assert!(event.component.contains(&format!("{}\r\n", hash_line(&event.hash))), "{}", event.component);
        assert!(event.component.contains("SUMMARY:Review"), "{}", event.component);
        assert!(event.component.contains("LOCATION:Room 1"), "{}", event.component);
    }

    fn hash_line(hash: &str) -> String {
        format!("{HASH_PROPERTY}:{hash}")
    }

    #[test]
    fn hash_tracks_the_copied_fields() {
        let base = build(&occurrence());
        type OccurrenceEdit = fn(&mut EventOccurrence);
        let cases: Vec<(&str, OccurrenceEdit)> = vec![
            ("summary", |occ| occ.summary = Some("Renamed".to_string())),
            ("summary cleared", |occ| occ.summary = None),
            ("start", |occ| occ.start += Duration::minutes(30)),
            ("end", |occ| occ.end += Duration::minutes(30)),
            ("all-day", |occ| occ.all_day = true),
            ("time zone", |occ| occ.time_zone = Some("Europe/Berlin".to_string())),
            ("location", |occ| occ.location = Some("Room 2".to_string())),
            ("location cleared", |occ| occ.location = None),
        ];
        for (name, change) in cases {
            let mut occ = occurrence();
            change(&mut occ);
            assert_ne!(base.hash, build(&occ).hash, "changing the {name} must change the hash");
        }
    }

    #[test]
    fn untracked_fields_do_not_change_the_hash() {
        let base = build(&occurrence());
        let mut occ = occurrence();
        occ.attendees = vec![Attendee {
            address: EmailAddress::new("someone@example.com"),
            role: AttendeeRole::Required,
            status: AttendeeStatus::Accepted,
        }];
        occ.organizer = Some(EmailAddress::new("me@example.com"));
        occ.categories = vec!["Work".to_string()];
        occ.reminder_minutes_before = Some(15);
        occ.conference_url = Some("https://meet.example.com/abc".to_string());
        occ.href = Some("/cal/evt-1.ics".to_string());
        occ.etag = Some("\"abc\"".to_string());
        occ.sequence = 7;
        occ.sensitivity = EventSensitivity::Private;
        occ.show_as = BusyStatus::Free;
        occ.status = EventStatus::Tentative;
        occ.rrule = Some("FREQ=WEEKLY".to_string());
        let changed = build(&occ);
        assert_eq!(base.hash, changed.hash, "fields that aren't copied must not force a rewrite");
        for property in ["ATTENDEE", "ORGANIZER", "CATEGORIES", "CONFERENCE", "VALARM", "CLASS", "STATUS", "SEQUENCE"] {
            assert!(!changed.component.contains(property), "{property} must not be copied:\n{}", changed.component);
        }
    }

    #[test]
    fn description_is_copied_only_when_allowed() {
        let occ = occurrence();
        let without = mirror_event(&occ, false, &hidden()).unwrap();
        assert!(!without.component.contains("DESCRIPTION"), "{}", without.component);

        let mut other_notes = occ.clone();
        other_notes.description = Some("Different notes".to_string());
        let without_other = mirror_event(&other_notes, false, &hidden()).unwrap();
        assert_eq!(without.hash, without_other.hash, "notes that aren't copied don't change the hash");

        let with = mirror_event(&occ, true, &hidden()).unwrap();
        assert!(with.component.contains("DESCRIPTION:Secret notes"), "{}", with.component);
        let with_other = mirror_event(&other_notes, true, &hidden()).unwrap();
        assert_ne!(with.hash, with_other.hash, "notes that are copied do change the hash");
        assert_eq!(with.uid, without.uid, "the privacy setting never changes an event's identity");
    }

    #[test]
    fn cancelled_and_hidden_occurrences_are_dropped() {
        let mut cancelled = occurrence();
        cancelled.status = EventStatus::Cancelled;
        assert!(mirror_event(&cancelled, false, &hidden()).is_none());

        let mut hidden_calendars = HashSet::new();
        hidden_calendars.insert(CalendarId("acct:cal".to_string()));
        assert!(mirror_event(&occurrence(), false, &hidden_calendars).is_none());

        hidden_calendars.insert(CalendarId("acct:other".to_string()));
        assert!(mirror_event(&occurrence(), false, &hidden_calendars).is_none(), "an event on any hidden calendar stays out");
        let mut elsewhere = occurrence();
        elsewhere.calendar_id = CalendarId("acct:visible".to_string());
        assert!(mirror_event(&elsewhere, false, &hidden_calendars).is_some());
    }

    #[test]
    fn plan_reports_creates_modifies_and_removes() {
        let base = build(&occurrence());
        let mut renamed = occurrence();
        renamed.summary = Some("Renamed".to_string());
        let modified = build(&renamed);
        assert_eq!(base.uid, modified.uid, "a rename keeps the mirror UID");
        assert_ne!(base.hash, modified.hash);

        let mut new_event = occurrence();
        new_event.uid = EventUid("evt-2@example.com".to_string());
        let created = build(&new_event);

        let plan = plan(
            &[modified, created.clone()],
            &[(base.uid.clone(), base.hash.clone()), ("lookout-gone".to_string(), "old".to_string())],
        );
        assert_eq!(plan.create, vec![created.uid]);
        assert_eq!(plan.modify, vec![base.uid]);
        assert_eq!(plan.remove, vec!["lookout-gone".to_string()]);
        assert!(!plan.is_empty());
    }

    #[test]
    fn plan_treats_a_moved_event_as_a_replacement() {
        let before = build(&occurrence());
        let mut moved = occurrence();
        moved.start += Duration::days(1);
        moved.end += Duration::days(1);
        let after = build(&moved);
        let plan = plan(std::slice::from_ref(&after), &[(before.uid.clone(), before.hash.clone())]);
        assert_eq!(plan.create, vec![after.uid]);
        assert_eq!(plan.remove, vec![before.uid]);
        assert!(plan.modify.is_empty(), "a moved occurrence gets a new stable UID, not an edit");
    }

    #[test]
    fn plan_is_empty_for_an_empty_or_unchanged_mirror() {
        assert!(plan(&[], &[]).is_empty());
        let event = build(&occurrence());
        assert!(plan(std::slice::from_ref(&event), &[(event.uid.clone(), event.hash.clone())]).is_empty());
    }

    #[test]
    fn existing_events_reads_back_uids_and_hashes() {
        let event = build(&occurrence());
        let calendar = build_calendar(std::slice::from_ref(&event));
        assert_eq!(existing_events(&calendar), vec![(event.uid, event.hash)]);
    }

    #[test]
    fn build_calendar_dedupes_timezones_and_orders_events() {
        let mut later = occurrence();
        later.time_zone = Some("Europe/Berlin".to_string());
        later.start = at(2026, 7, 15, 16);
        later.end = at(2026, 7, 15, 17);
        let later = build(&later);

        let mut earlier = occurrence();
        earlier.uid = EventUid("evt-early@example.com".to_string());
        earlier.time_zone = Some("Europe/Berlin".to_string());
        earlier.start = at(2026, 7, 15, 9);
        earlier.end = at(2026, 7, 15, 10);
        let earlier = build(&earlier);

        let calendar = build_calendar(&[later.clone(), earlier.clone()]);
        assert!(calendar.starts_with("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n"), "{calendar}");
        assert!(calendar.ends_with("END:VCALENDAR\r\n"), "{calendar}");
        assert_eq!(calendar.matches("BEGIN:VTIMEZONE").count(), 1, "one VTIMEZONE per zone:\n{calendar}");
        let earlier_at = calendar.find(&earlier.uid).expect("earlier event present");
        let later_at = calendar.find(&later.uid).expect("later event present");
        assert!(earlier_at < later_at, "events are ordered by start");
    }

    #[test]
    fn the_window_covers_its_months() {
        let now = Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap();
        assert_eq!(
            mirror_months(now),
            vec![
                NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
            ]
        );
        assert!(month_overlaps_window(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(), now));
        assert!(month_overlaps_window(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(), now));
        assert!(!month_overlaps_window(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(), now));
        assert!(!month_overlaps_window(NaiveDate::from_ymd_opt(2027, 1, 1).unwrap(), now));
    }

    #[test]
    fn the_window_drops_far_past_cancelled_and_hidden_events() {
        let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let mut inside = occurrence();
        inside.start = now + Duration::hours(1);
        inside.end = inside.start + Duration::hours(1);
        let inside_uid = mirror_uid(&inside);

        let mut past = inside.clone();
        past.uid = EventUid("past".to_string());
        past.start = now - Duration::days(10);
        past.end = past.start + Duration::hours(1);

        let mut far = inside.clone();
        far.uid = EventUid("far".to_string());
        far.start = now + Duration::days(70);
        far.end = far.start + Duration::hours(1);

        let mut cancelled = inside.clone();
        cancelled.uid = EventUid("cancelled".to_string());
        cancelled.status = EventStatus::Cancelled;

        let mut hidden = inside.clone();
        hidden.uid = EventUid("hidden".to_string());
        hidden.calendar_id = CalendarId("acct:hidden".to_string());

        let mut hidden_calendars = HashSet::new();
        hidden_calendars.insert(CalendarId("acct:hidden".to_string()));
        let events = mirror_events_from(&[inside, past, far, cancelled, hidden], now, false, &hidden_calendars);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, inside_uid);
    }

    #[test]
    fn sync_writes_until_the_mirror_matches() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let account_id = AccountId(format!("eds-mirror-test-{}-{stamp}", std::process::id()));
        let now = Utc::now();
        let month = first_of_month(now.date_naive());
        let cache = lookout_dav::CalendarCache::open(&account_id).expect("test calendar cache opens");

        let mut inside = occurrence();
        inside.calendar_id = CalendarId("acct:cal".to_string());
        inside.uid = EventUid("inside".to_string());
        inside.summary = Some("Inside".to_string());
        inside.start = now + Duration::hours(1);
        inside.end = now + Duration::hours(2);

        let mut far = inside.clone();
        far.uid = EventUid("far".to_string());
        far.summary = Some("Far away".to_string());
        far.start = now + Duration::days(100);
        far.end = far.start + Duration::hours(1);
        cache.store_month(month, &[inside, far.clone()]).unwrap();

        let dir = std::env::temp_dir().join(format!("lookout-eds-mirror-{}-{stamp}", std::process::id()));
        let path = dir.join("eds-mirror.ics");
        let config = MirrorConfig {
            sources: vec![account_id],
            hidden_calendars: HashSet::new(),
            include_description: false,
        };

        let first = sync_mirror_blocking(&config, &path).expect("first sync writes");
        assert!(first.written);
        assert_eq!(first.events, 1);
        let calendar = std::fs::read_to_string(&path).unwrap();
        assert!(calendar.contains("SUMMARY:Inside"), "{calendar}");
        assert!(!calendar.contains("Far away"), "{calendar}");

        let second = sync_mirror_blocking(&config, &path).expect("second sync is a no-op");
        assert!(!second.written);
        assert!(second.plan.is_empty());

        cache.store_month(month, &[far]).unwrap();
        let third = sync_mirror_blocking(&config, &path).expect("removal rewrites");
        assert!(third.written);
        assert_eq!(third.events, 0);
        assert_eq!(third.plan.remove.len(), 1);
        let calendar = std::fs::read_to_string(&path).unwrap();
        assert!(!calendar.contains("Inside"), "{calendar}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_state_coalesces_a_request_made_while_running() {
        let mut state = MirrorSyncState {
            enabled: true,
            ..Default::default()
        };
        assert!(state.begin(), "the first request starts a run");
        assert!(!state.begin(), "a second request coalesces");
        assert!(state.finish(), "finishing reports the follow-up");
        assert!(!state.finish(), "only one follow-up run");

        let mut disabled = MirrorSyncState::default();
        assert!(!disabled.begin());
    }
}
