//! Calendar and task UI glue: editors, peeks, event menus, save/delete routes, checklists, dashboard and mail-overview refreshes, and the Add-calendar dialog.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use chrono::Datelike;
use gtk::{gio, glib};
use lookout_core::calendar_protocol::CalendarCommand;
use lookout_core::{AccountId, CalendarEvent, CalendarId, CalendarInfo, CalendarTask, EmailAddress, EventOccurrence, EventUid, SchedulingInfo, WebcalSubscription};
use lookout_dav::subscription::SubscriptionCommand;

use crate::calendar_colors;
use crate::calendar_view::{self, CalendarMain};
use crate::contacts_view::calendar_attendee_suggestions;
use crate::google_tasks::{self};
use crate::worker::Worker;

use super::*;

/// Re-anchors the main calendar panel's already-redrawn views, keeps the
/// sidebar's mini-calendar showing the anchor's month (with event-day
/// markers), records the displayed month, and asks every connected calendar
/// account to resync it.
pub(crate) fn show_anchor(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_main: &CalendarMain, mini_calendar: &calendar_view::MiniCalendar) {
    let day = calendar_view::anchor(calendar_main);
    let month = first_of_month(day);
    let mut st = calendar_state.borrow_mut();
    st.displayed_month = month;
    // The birthdays calendar has no session to ask - recompute the month's
    // occurrences in place so the mini-calendar's event-day markers (and any
    // surface reading `checked_occurrences`) see them immediately.
    if let Some(birthdays) = &mut st.birthdays {
        birthdays.sync_month(month);
    }
    for handle in st.accounts.values() {
        let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(month));
    }
    let webcal_cmd = st.webcal_cmd_tx.clone();
    if let Some(cmd) = webcal_cmd {
        let _ = cmd.send_blocking(SubscriptionCommand::SyncMonth(month));
    }
    drop(st);
    let event_days = calendar_event_days(calendar_state, month);
    calendar_view::set_mini_month(mini_calendar, day, &event_days);
    // The header's current-month dropdown holds its own mini grid; keep it
    // showing the same month as the main panel so the dropdown is never stale.
    calendar_view::set_mini_month(&calendar_main.month_mini, day, &event_days);
}

/// The local dates within `month` that have at least one occurrence from a
/// currently-checked calendar, unioned across every account that has synced
/// that month - drives the mini-calendar's bold event-day numerals. Every
/// date a multi-day occurrence covers counts, so an event spanning several
/// days marks each of them (matching the main month grid).
pub(crate) fn calendar_event_days(calendar_state: &Rc<RefCell<CalendarUiState>>, month: chrono::NaiveDate) -> HashSet<chrono::NaiveDate> {
    let st = calendar_state.borrow();
    let mut days = HashSet::new();
    let month_start = first_of_month(month);
    let month_end = last_of_month(month);
    for occ in st.checked_occurrences(month) {
        days.extend(calendar_view::covered_local_dates(occ, month_start, month_end));
    }
    days
}

pub(crate) fn current_month_start() -> chrono::NaiveDate {
    first_of_month(crate::calendar_tz::display_zone().today())
}

pub(crate) fn first_of_month(date: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    date.with_day(1).unwrap_or(date)
}

/// The last day of `date`'s month.
pub(crate) fn last_of_month(date: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    let first = first_of_month(date);
    let next = if first.month() == 12 {
        chrono::NaiveDate::from_ymd_opt(first.year() + 1, 1, 1).unwrap_or(first)
    } else {
        chrono::NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1).unwrap_or(first)
    };
    next - chrono::Duration::days(1)
}

/// Maps a (nav-rail module, ribbon tab) pair to the `view_toolbar_stack`
/// child to show. Mail is tabbed - Home shows the command toolbar, Send /
/// Receive the account sync, Folder the open folder's commands, View the
/// layout toggles and Help the program's own; Calendar/Contacts each have a
/// single non-tabbed toolbar of their own, so they ignore the tab. Unknown
/// combos fall back to Mail-Home.
pub(crate) fn ribbon_stack_name(module: &str, tab: &str) -> &'static str {
    match (module, tab) {
        ("mail", "send-receive") => "mail-send-receive",
        ("mail", "folder") => "mail-folder",
        ("mail", "view") => "mail-view",
        ("mail", "help") => "mail-help",
        ("mail", _) => "mail-home",
        ("calendar", _) => "calendar",
        ("tasks", _) => "tasks",
        ("contacts", _) => "contacts",
        ("lookout", _) => "lookout",
        _ => "mail-home",
    }
}

/// One handle's occurrences for `first_of_month`: the dashboard cache's month
/// bucket when present, else the handle's last-synced month, else nothing.
pub(crate) fn month_occurrences_from<'a>(
    by_month: &'a HashMap<chrono::NaiveDate, Vec<EventOccurrence>>,
    last_synced_month: Option<chrono::NaiveDate>,
    last_occurrences: &'a [EventOccurrence],
    first_of_month: chrono::NaiveDate,
) -> Vec<&'a EventOccurrence> {
    match by_month.get(&first_of_month) {
        Some(occurrences) => occurrences.iter().collect(),
        None if last_synced_month == Some(first_of_month) => last_occurrences.iter().collect(),
        None => Vec::new(),
    }
}

impl CalendarUiState {
    /// Every checked calendar's latest occurrences for `month`, unioned
    /// across accounts, webcal feeds, and the birthdays calendar - the one
    /// merge every calendar-facing surface (main view, print, mini-calendar
    /// event days, agenda) uses. Callers must have synced the birthdays
    /// handle for `month` first (the contacts hook and `show_anchor` do).
    pub(crate) fn checked_occurrences(&self, month: chrono::NaiveDate) -> Vec<&EventOccurrence> {
        self.accounts
            .values()
            .filter(|h| h.last_synced_month == Some(month))
            .flat_map(|h| h.last_occurrences.iter())
            .chain(
                self.webcal_handles
                    .values()
                    .filter(|h| h.last_synced_month == Some(month))
                    .flat_map(|h| h.last_occurrences.iter()),
            )
            .chain(
                self.birthdays
                    .as_ref()
                    .filter(|h| h.last_synced_month == Some(month))
                    .into_iter()
                    .flat_map(|h| h.last_occurrences.iter()),
            )
            .filter(|occ| self.checked_calendar_ids.contains(&occ.calendar_id))
            .collect()
    }

    /// Every checked calendar's occurrences for `month`, preferring the
    /// dashboard's multi-month cache (`occurrences_by_month`) over a handle's
    /// single `last_occurrences` - so the invite card's conflict check
    /// (CAL-35) sees the month even when it isn't the one a handle last
    /// synced. Falls back to nothing when the month isn't loaded.
    pub(crate) fn checked_occurrences_for_month(&self, month: chrono::NaiveDate) -> Vec<&EventOccurrence> {
        let first = month.with_day(1).unwrap_or(month);
        let mut out: Vec<&EventOccurrence> = Vec::new();
        for handle in self.accounts.values() {
            out.extend(month_occurrences_from(
                &handle.occurrences_by_month,
                handle.last_synced_month,
                &handle.last_occurrences,
                first,
            ));
        }
        for handle in self.webcal_handles.values() {
            out.extend(month_occurrences_from(
                &handle.occurrences_by_month,
                handle.last_synced_month,
                &handle.last_occurrences,
                first,
            ));
        }
        if let Some(handle) = &self.birthdays {
            out.extend(month_occurrences_from(
                &handle.occurrences_by_month,
                handle.last_synced_month,
                &handle.last_occurrences,
                first,
            ));
        }
        out.into_iter().filter(|occ| self.checked_calendar_ids.contains(&occ.calendar_id)).collect()
    }

    /// Like [`Self::checked_occurrences`], but across every synced month -
    /// the surfaces that filter by exact day (the mail-overview day list,
    /// the event editor's preview) rather than by the displayed month.
    pub(crate) fn checked_occurrences_all_months(&self) -> Vec<&EventOccurrence> {
        self.accounts
            .values()
            .flat_map(|h| h.last_occurrences.iter())
            .chain(self.webcal_handles.values().flat_map(|h| h.last_occurrences.iter()))
            .chain(self.birthdays.as_ref().into_iter().flat_map(|h| h.last_occurrences.iter()))
            .filter(|occ| self.checked_calendar_ids.contains(&occ.calendar_id))
            .collect()
    }

    /// Whether `calendar_id` is a read-only synthetic calendar (a webcal
    /// feed, or the birthdays calendar) - shared by the editor's read-only
    /// mode, the drag guard, and the delete route, so one id-based check
    /// covers every "no write-back path" source.
    pub(crate) fn is_read_only_calendar(&self, calendar_id: &CalendarId) -> bool {
        self.read_only_note(calendar_id).is_some()
    }

    /// The read-only explanation the event editor shows under the form for a
    /// synthetic calendar, or `None` when the calendar is writable.
    pub(crate) fn read_only_note(&self, calendar_id: &CalendarId) -> Option<&'static str> {
        if self.webcal_handles.values().any(|h| &h.calendar_id == calendar_id) {
            Some("This calendar is a read-only subscription - changes can't be saved back to the feed.")
        } else if self.birthdays.as_ref().is_some_and(|h| &h.calendar_id == calendar_id) {
            Some("This birthday is synthesized from your contacts - it can't be edited or deleted.")
        } else {
            None
        }
    }
}

/// Copies the current contacts snapshots into the birthdays handle
/// (creating it on the first contact, dropping it when the last contact
/// goes away) and recomputes the displayed month plus the dashboard
/// horizon, then feeds the reminder engine the freshly-computed
/// occurrences - the `OccurrencesUpdated`-ingest equivalent for a calendar
/// that has no session of its own. Returns whether any contacts exist (the
/// caller only runs the render funnel when they do).
pub(crate) fn refresh_birthdays_from_contacts(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    reminders_engine: &Rc<RefCell<crate::reminders::ReminderEngine>>,
) -> bool {
    let contacts: Vec<(AccountId, Vec<lookout_dav::ContactRecord>)> = {
        let st = state.borrow();
        st.contacts_by_account
            .iter()
            .map(|(account_id, snapshot)| (account_id.clone(), snapshot.contacts.clone()))
            .collect()
    };
    if contacts.is_empty() {
        // No contacts anywhere - hide the calendar (the checklist row
        // disappears with it) rather than show an empty "Birthdays" row.
        calendar_state.borrow_mut().birthdays = None;
        return false;
    }
    let mut st = calendar_state.borrow_mut();
    let month = st.displayed_month;
    let handle = st.birthdays.get_or_insert_with(|| BirthdaysHandle {
        calendar_id: birthdays_calendar_id(),
        display_name: "Birthdays".to_string(),
        contacts: Vec::new(),
        last_occurrences: Vec::new(),
        last_synced_month: None,
        occurrences_by_month: HashMap::new(),
    });
    handle.set_contacts(contacts);
    handle.sync_month(month);
    handle.sync_dashboard_window();
    drop(st);
    // Ingest both the displayed month and the dashboard horizon, so an alert
    // for an upcoming month's birthday is already in the engine before the
    // calendar view ever navigates there (the same union the CalDAV
    // sessions' `OccurrencesUpdated` ingests build up month by month).
    let occurrences: Vec<EventOccurrence> = {
        let st = calendar_state.borrow();
        let handle = st.birthdays.as_ref().expect("created just above");
        handle
            .last_occurrences
            .iter()
            .cloned()
            .chain(handle.occurrences_by_month.values().flatten().cloned())
            .collect()
    };
    reminders_engine.borrow_mut().ingest(&AccountId("birthdays".to_string()), &occurrences);
    true
}

/// Recomputes which calendars actually exist across every connected account,
/// defaults any newly-seen id to checked (shown), and re-renders the
/// sidebar's "My calendars" checklist against that - the checklist's own
/// `on_toggle` closure flips membership in `checked_calendar_ids` and calls
/// `refresh_displayed_calendar_view` to redraw the grid accordingly. Calendars
/// the user removed from the list (item 3.13) are filtered out of both the
/// checklist and the views until "Show all" reveals them; the sections come
/// from the user's groups first, then the per-account fallback.
pub(crate) fn refresh_calendar_checklist(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_list_box: &gtk::Box, calendar_main: &Rc<CalendarMain>) {
    let (groups, hidden, on_action) = {
        let st = calendar_state.borrow();
        (st.calendar_groups.clone(), st.hidden_calendars.clone(), st.calendar_sidebar_action.clone())
    };
    let hidden_set: HashSet<CalendarId> = hidden.iter().cloned().collect();

    let mut fallbacks: Vec<calendar_view::CalendarFallbackSection> = calendar_state
        .borrow()
        .accounts
        .values()
        .map(|h| calendar_view::CalendarFallbackSection {
            title: h.display_name.clone(),
            calendars: h.calendars.clone(),
            status: calendar_view::calendar_account_status_text(&h.connection_state, !h.calendars.is_empty()),
        })
        .collect();
    // Subscriptions render as one synthetic "Webcal subscriptions" section so
    // they inherit the checklist's toggle/color machinery unchanged.
    {
        let st = calendar_state.borrow();
        if !st.webcal_subscriptions.is_empty() {
            fallbacks.push(calendar_view::CalendarFallbackSection {
                title: "Webcal subscriptions".to_string(),
                calendars: st
                    .webcal_subscriptions
                    .iter()
                    .map(|sub| CalendarInfo {
                        id: webcal_calendar_id(&sub.id),
                        account_id: AccountId(format!("webcal:{}", sub.id)),
                        display_name: sub.display_name.clone(),
                        color: None,
                        href: sub.url.clone(),
                        // Feeds are events-only - never a task target.
                        supports_tasks: false,
                        owner: None,
                    })
                    .collect(),
                status: None,
            });
        }
    }
    // The synthesized birthdays calendar renders as one row in its own
    // section, present only while any contacts exist (the handle is `None`
    // otherwise) - same inheritance of the toggle/color machinery.
    {
        let st = calendar_state.borrow();
        if let Some(birthdays) = &st.birthdays {
            fallbacks.push(calendar_view::CalendarFallbackSection {
                title: "Birthdays".to_string(),
                calendars: vec![CalendarInfo {
                    id: birthdays.calendar_id.clone(),
                    account_id: AccountId("birthdays".to_string()),
                    display_name: birthdays.display_name.clone(),
                    color: None,
                    href: String::new(),
                    // Events-only, like feeds - never a task target.
                    supports_tasks: false,
                    owner: None,
                }],
                status: None,
            });
        }
    }
    fallbacks.sort_by_key(|g| g.title.to_lowercase());

    // Count the removed calendars that still exist somewhere (a stale hidden id
    // for a calendar the server no longer offers shouldn't keep a "Show all"
    // row alive), then drop them from every section - so a removed calendar is
    // out of both the checklist and the views.
    let hidden_count = fallbacks.iter().flat_map(|f| f.calendars.iter()).filter(|c| hidden_set.contains(&c.id)).count();
    for fallback in &mut fallbacks {
        fallback.calendars.retain(|c| !hidden_set.contains(&c.id));
    }

    let choices: Vec<calendar_view::CalendarGroupChoice> = groups
        .iter()
        .map(|g| calendar_view::CalendarGroupChoice {
            id: g.id.clone(),
            name: g.name.clone(),
            calendar_ids: g.calendar_ids.clone(),
        })
        .collect();
    let sections = calendar_view::build_calendar_sections(&fallbacks, &choices);

    let all_calendars: Vec<CalendarInfo> = fallbacks.iter().flat_map(|g| g.calendars.iter().cloned()).collect();
    {
        let mut st = calendar_state.borrow_mut();
        ensure_checked_calendars(&mut st.checked_calendar_ids, &all_calendars);
        calendar_colors::assign_missing(&mut st.calendar_colors, &all_calendars);
    }
    let checked = calendar_state.borrow().checked_calendar_ids.clone();
    let colors = calendar_state.borrow().calendar_colors.clone();
    let on_toggle = {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let all_calendars = all_calendars.clone();
        move |id: CalendarId, is_checked: bool| {
            {
                let mut st = calendar_state.borrow_mut();
                if is_checked {
                    st.checked_calendar_ids.insert(id);
                } else {
                    st.checked_calendar_ids.remove(&id);
                }
            }
            // The toggle takes the quick refresh path, skipping this checklist
            // rebuild, so recompute the Schedule view's lanes here (CAL-03).
            let checked = calendar_state.borrow().checked_calendar_ids.clone();
            calendar_view::set_schedule_calendars(&calendar_main, &schedule_calendars_from(&all_calendars, &checked));
            refresh_displayed_calendar_view(&calendar_state, &calendar_main);
        }
    };
    let on_action = on_action.unwrap_or_else(|| Rc::new(|_| {}));
    calendar_view::rebuild_calendar_checklist(
        calendar_list_box,
        &sections,
        &checked,
        &colors,
        &calendar_main.check_colors,
        on_toggle,
        on_action,
        &choices,
        hidden_count,
    );
    calendar_view::set_calendar_colors(calendar_main, &colors);
    calendar_view::set_schedule_calendars(calendar_main, &schedule_calendars_from(&all_calendars, &checked));
    refresh_displayed_calendar_view(calendar_state, calendar_main);
}

/// The checked calendars in sidebar order, as `(id, display_name)`, for the
/// Schedule view's lanes (CAL-03) - an empty calendar still gets a row.
pub(crate) fn schedule_calendars_from(all_calendars: &[CalendarInfo], checked: &HashSet<CalendarId>) -> Vec<(CalendarId, String)> {
    all_calendars
        .iter()
        .filter(|c| checked.contains(&c.id))
        .map(|c| (c.id.clone(), c.display_name.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Calendar sidebar actions (item 3.13 / CAL-32, CAL-33)
// ---------------------------------------------------------------------------

/// Every calendar a connected source currently offers, with hidden ones
/// optionally included - the lookup the sidebar action handler resolves ids
/// against, and (sorted by id) the position `Automatic colour` scores from.
fn calendars_in_state(calendar_state: &Rc<RefCell<CalendarUiState>>, include_hidden: bool) -> Vec<CalendarInfo> {
    let st = calendar_state.borrow();
    let mut out: Vec<CalendarInfo> = st.accounts.values().flat_map(|h| h.calendars.iter().cloned()).collect();
    for sub in &st.webcal_subscriptions {
        out.push(CalendarInfo {
            id: webcal_calendar_id(&sub.id),
            account_id: AccountId(format!("webcal:{}", sub.id)),
            display_name: sub.display_name.clone(),
            color: None,
            href: sub.url.clone(),
            supports_tasks: false,
            owner: None,
        });
    }
    if let Some(birthdays) = &st.birthdays {
        out.push(CalendarInfo {
            id: birthdays.calendar_id.clone(),
            account_id: AccountId("birthdays".to_string()),
            display_name: birthdays.display_name.clone(),
            color: None,
            href: String::new(),
            supports_tasks: false,
            owner: None,
        });
    }
    if !include_hidden {
        let hidden: HashSet<CalendarId> = st.hidden_calendars.iter().cloned().collect();
        out.retain(|c| !hidden.contains(&c.id));
    }
    out
}

/// The account whose session owns `calendar_id`, as `(account id, command
/// channel, is_graph)`. `None` for a Webcal subscription or the synthesized
/// Birthdays calendar - they have no session to talk to.
fn calendar_session_for(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> Option<(AccountId, async_channel::Sender<CalendarCommand>, bool)> {
    let st = calendar_state.borrow();
    st.accounts
        .iter()
        .find(|(_, handle)| handle.calendars.iter().any(|c| &c.id == calendar_id))
        .map(|(id, handle)| (id.clone(), handle.cmd_tx.clone(), handle.graph))
}

/// The `webcal:<id>` subscription id behind a synthetic calendar id, if any.
fn webcal_subscription_id(calendar_id: &CalendarId) -> Option<&str> {
    calendar_id.0.strip_prefix("webcal:")
}

/// The colour currently shown for `calendar_id` (what a colour's Undo
/// restores).
fn current_calendar_color(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> String {
    calendar_state
        .borrow()
        .calendar_colors
        .get(calendar_id)
        .cloned()
        .unwrap_or_else(|| calendar_colors::DEFAULT_CHECK_COLOR.to_string())
}

/// Applies a colour locally and persists the map - the optimistic half of a
/// pick, before any server write-back.
fn set_calendar_color_local(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId, color: &str) {
    calendar_state.borrow_mut().calendar_colors.insert(calendar_id.clone(), color.to_string());
    calendar_colors::save(&calendar_state.borrow().calendar_colors);
}

/// Sends the colour to the owning CalDAV session so the server learns it
/// (`PROPPATCH calendar-color`, item 3.13 / CAL-32). A Microsoft 365 calendar
/// keeps it on the device (Graph's `hexColor` is read-only), and a Webcal or
/// Birthdays calendar has no session at all.
fn write_calendar_color_back(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId, color: &str) {
    if let Some((_, tx, graph)) = calendar_session_for(calendar_state, calendar_id) {
        if !graph {
            let _ = tx.send_blocking(CalendarCommand::SetCalendarColor {
                calendar_id: calendar_id.clone(),
                color: color.to_string(),
            });
        }
    }
}

/// Applies a picked colour end to end: locally, to the server (CalDAV), to
/// every surface, and with an Undo that restores the previous colour.
fn apply_calendar_color(calendar_state: &Rc<RefCell<CalendarUiState>>, toast_overlay: &adw::ToastOverlay, refresh: &Rc<dyn Fn()>, calendar_id: &CalendarId, color: &str) {
    let previous = current_calendar_color(calendar_state, calendar_id);
    set_calendar_color_local(calendar_state, calendar_id, color);
    write_calendar_color_back(calendar_state, calendar_id, color);
    refresh();
    let toast = adw::Toast::new("Colour updated");
    toast.set_button_label(Some("Undo"));
    {
        let calendar_state = calendar_state.clone();
        let refresh = refresh.clone();
        let calendar_id = calendar_id.clone();
        let previous = previous.clone();
        toast.connect_button_clicked(move |_| {
            set_calendar_color_local(&calendar_state, &calendar_id, &previous);
            write_calendar_color_back(&calendar_state, &calendar_id, &previous);
            refresh();
        });
    }
    toast_overlay.add_toast(toast);
}

/// Repaints every calendar surface after a sidebar change (item 3.13): the
/// checklist and main panel (via `refresh_calendar_checklist`), the Mail
/// overview pane and the sidebar mini-calendar. The Add-calendar dialog's own
/// refresh and the row/group actions share it, so one change looks the same
/// wherever it was made.
pub(crate) fn refresh_calendar_surfaces(
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    calendar_main: &Rc<CalendarMain>,
    calendar_list_box: &gtk::Box,
    mini_calendar: &calendar_view::MiniCalendar,
    mail_overview_day: &Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: &gtk::Box,
) {
    refresh_calendar_checklist(calendar_state, calendar_list_box, calendar_main);
    refresh_mail_overview_day_list(calendar_state, mail_overview_day.get(), mail_overview_day_list);
    let month = calendar_state.borrow().displayed_month;
    let event_days = calendar_event_days(calendar_state, month);
    calendar_view::set_mini_event_days(mini_calendar, &event_days);
}

/// Removes a Webcal subscription (item 3.13's row-menu Remove for a feed, and
/// the Add-calendar dialog's own manage list): drops it from `settings.json`
/// and the in-memory mirrors, deletes its cache, and reloads the feed session.
/// The caller repaints. Unlike hiding a calendar this is destructive (the
/// cached events go), so callers confirm first.
pub(crate) fn remove_webcal_subscription(state: &Rc<RefCell<UiState>>, calendar_state: &Rc<RefCell<CalendarUiState>>, toast_overlay: &adw::ToastOverlay, id: &str) {
    let subscriptions = {
        let app_config = state.borrow().app_config.clone();
        let mut config = app_config.borrow_mut();
        config.webcal_subscriptions.retain(|s| s.id != id);
        crate::app_config::save(&config);
        config.webcal_subscriptions.clone()
    };
    {
        let mut st = calendar_state.borrow_mut();
        st.webcal_subscriptions.retain(|s| s.id != id);
        st.webcal_handles.remove(id);
        st.checked_calendar_ids.remove(&webcal_calendar_id(id));
        st.calendar_colors.remove(&webcal_calendar_id(id));
    }
    calendar_colors::save(&calendar_state.borrow().calendar_colors);
    let _ = lookout_dav::remove_subscription_cache(id);
    if let Some(cmd) = calendar_state.borrow().webcal_cmd_tx.clone() {
        let _ = cmd.send_blocking(SubscriptionCommand::Reload { subscriptions });
    }
    toast_overlay.add_toast(adw::Toast::new("Subscription removed"));
}

/// Registers the sidebar checklist's row/group action handler (item 3.13 /
/// CAL-32, CAL-33) on `calendar_state`, the late-binding pattern the other
/// window hooks use - so `refresh_calendar_checklist` can hand it to every
/// rebuild from the many call sites that have no window reference.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_calendar_sidebar_actions(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    calendar_main: &Rc<CalendarMain>,
    calendar_list_box: &gtk::Box,
    mini_calendar: &calendar_view::MiniCalendar,
    mail_overview_day: &Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: &gtk::Box,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    toast_overlay: &adw::ToastOverlay,
) {
    // One repaint closure for every action: the checklist/main panel, the Mail
    // overview pane, the sidebar mini-calendar, the Tasks list (its rows are
    // colour-dotted) and the dashboard's upcoming-events card.
    let refresh: Rc<dyn Fn()> = Rc::new({
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let calendar_list_box = calendar_list_box.clone();
        let mini_calendar = mini_calendar.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let tasks_view = tasks_view.clone();
        move || {
            refresh_calendar_surfaces(
                &calendar_state,
                &calendar_main,
                &calendar_list_box,
                &mini_calendar,
                &mail_overview_day,
                &mail_overview_day_list,
            );
            refresh_tasks_view(&calendar_state, &tasks_view);
            let dashboard_refresh = calendar_state.borrow().dashboard_refresh.clone();
            if let Some(hook) = dashboard_refresh {
                hook();
            }
        }
    });

    /// Persists `AppConfig` and refreshes `CalendarUiState`'s group/hidden
    /// mirrors from it - the shared tail of every group/hide action.
    fn persist(state: &Rc<RefCell<UiState>>, calendar_state: &Rc<RefCell<CalendarUiState>>) {
        let app_config = state.borrow().app_config.clone();
        let config = app_config.borrow();
        crate::app_config::save(&config);
        let mut st = calendar_state.borrow_mut();
        st.calendar_groups = config.calendar_groups.clone();
        st.hidden_calendars = config.hidden_calendars.clone();
    }

    let handler: calendar_view::CalendarSidebarActionHandler = Rc::new({
        let window = window.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let refresh = refresh.clone();
        move |action| {
            match action {
                calendar_view::CalendarSidebarAction::SetColor(calendar_id, color) => {
                    apply_calendar_color(&calendar_state, &toast_overlay, &refresh, &calendar_id, &color);
                }
                calendar_view::CalendarSidebarAction::CustomColor(calendar_id) => {
                    let dialog = gtk::ColorDialog::new();
                    let initial = gtk::gdk::RGBA::parse(current_calendar_color(&calendar_state, &calendar_id)).ok();
                    let calendar_state = calendar_state.clone();
                    let toast_overlay = toast_overlay.clone();
                    let refresh = refresh.clone();
                    let window = window.clone();
                    dialog.choose_rgba(Some(&window), initial.as_ref(), gtk::gio::Cancellable::NONE, move |result| {
                        if let Ok(rgba) = result {
                            let color = calendar_colors::rgba_to_hex(rgba.red(), rgba.green(), rgba.blue());
                            apply_calendar_color(&calendar_state, &toast_overlay, &refresh, &calendar_id, &color);
                        }
                    });
                }
                calendar_view::CalendarSidebarAction::AutomaticColor(calendar_id) => {
                    // The palette position is the calendar's place in the
                    // id-sorted list, exactly as `assign_missing` scores a
                    // first sighting.
                    let mut calendars = calendars_in_state(&calendar_state, true);
                    calendars.sort_by_key(|c| c.id.0.clone());
                    let (server, index) = calendars
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.id == calendar_id)
                        .map(|(index, c)| (c.color.clone(), index))
                        .unwrap_or((None, 0));
                    let color = calendar_colors::automatic_color(server.as_deref(), index);
                    apply_calendar_color(&calendar_state, &toast_overlay, &refresh, &calendar_id, &color);
                }
                calendar_view::CalendarSidebarAction::Rename(calendar_id) => {
                    if let Some(subscription_id) = webcal_subscription_id(&calendar_id).map(str::to_string) {
                        let initial = calendar_state
                            .borrow()
                            .webcal_subscriptions
                            .iter()
                            .find(|s| s.id == subscription_id)
                            .map(|s| s.display_name.clone())
                            .unwrap_or_default();
                        let state = state.clone();
                        let calendar_state = calendar_state.clone();
                        let refresh = refresh.clone();
                        crate::calendar_groups::show_name_dialog(&window, "Rename calendar", "Name", "Rename", &initial, move |name| {
                            let subscriptions = {
                                let app_config = state.borrow().app_config.clone();
                                let mut config = app_config.borrow_mut();
                                if let Some(sub) = config.webcal_subscriptions.iter_mut().find(|s| s.id == subscription_id) {
                                    sub.display_name = name.clone();
                                }
                                crate::app_config::save(&config);
                                config.webcal_subscriptions.clone()
                            };
                            calendar_state.borrow_mut().webcal_subscriptions = subscriptions.clone();
                            if let Some(cmd) = calendar_state.borrow().webcal_cmd_tx.clone() {
                                let _ = cmd.send_blocking(SubscriptionCommand::Reload { subscriptions });
                            }
                            refresh();
                        });
                    } else if let Some((_, tx, _)) = calendar_session_for(&calendar_state, &calendar_id) {
                        let initial = calendars_in_state(&calendar_state, true)
                            .into_iter()
                            .find(|c| c.id == calendar_id)
                            .map(|c| c.display_name)
                            .unwrap_or_default();
                        crate::calendar_groups::show_name_dialog(&window, "Rename calendar", "Name", "Rename", &initial, move |name| {
                            let _ = tx.send_blocking(CalendarCommand::UpdateCalendar {
                                calendar_id: calendar_id.clone(),
                                display_name: name,
                            });
                        });
                    } else {
                        toast_overlay.add_toast(adw::Toast::new("This calendar can't be renamed"));
                    }
                }
                calendar_view::CalendarSidebarAction::Share(calendar_id) => {
                    let shareable = calendar_session_for(&calendar_state, &calendar_id).map(|(_, _, graph)| graph).unwrap_or(false);
                    let calendar = calendars_in_state(&calendar_state, true).into_iter().find(|c| c.id == calendar_id);
                    match (shareable, calendar) {
                        // Only a calendar the user owns can be shared; a
                        // delegated one stays read-through.
                        (true, Some(calendar)) if calendar.owner.is_none() => crate::calendar_share::show_share_calendar_dialog(&window, calendar, &toast_overlay),
                        _ => toast_overlay.add_toast(adw::Toast::new("Sharing is available for Microsoft 365 calendars")),
                    }
                }
                calendar_view::CalendarSidebarAction::Remove(calendar_id) => {
                    if let Some(subscription_id) = webcal_subscription_id(&calendar_id).map(str::to_string) {
                        let name = calendars_in_state(&calendar_state, true)
                            .into_iter()
                            .find(|c| c.id == calendar_id)
                            .map(|c| c.display_name)
                            .unwrap_or_else(|| "this calendar".to_string());
                        let dialog = adw::MessageDialog::builder()
                            .transient_for(&window)
                            .modal(true)
                            .heading(format!("Unsubscribe from \"{name}\"?"))
                            .body("Its cached events will be removed. You can subscribe again later.")
                            .build();
                        dialog.add_response("cancel", "Cancel");
                        dialog.add_response("remove", "Unsubscribe");
                        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                        dialog.set_default_response(Some("cancel"));
                        dialog.set_close_response("cancel");
                        {
                            let state = state.clone();
                            let calendar_state = calendar_state.clone();
                            let toast_overlay = toast_overlay.clone();
                            let refresh = refresh.clone();
                            dialog.connect_response(None, move |_dialog, response| {
                                if response == "remove" {
                                    remove_webcal_subscription(&state, &calendar_state, &toast_overlay, &subscription_id);
                                    refresh();
                                }
                            });
                        }
                        dialog.present();
                    } else {
                        {
                            let app_config = state.borrow().app_config.clone();
                            let mut config = app_config.borrow_mut();
                            config.hide_calendar(&calendar_id);
                        }
                        persist(&state, &calendar_state);
                        // A hidden calendar must stop feeding the views: drop
                        // it from the checked set so its events vanish too.
                        calendar_state.borrow_mut().checked_calendar_ids.remove(&calendar_id);
                        refresh();
                        let name = calendars_in_state(&calendar_state, true)
                            .into_iter()
                            .find(|c| c.id == calendar_id)
                            .map(|c| c.display_name)
                            .unwrap_or_else(|| "Calendar".to_string());
                        let toast = adw::Toast::new(&format!("{name} removed from the list"));
                        toast.set_button_label(Some("Undo"));
                        {
                            let state = state.clone();
                            let calendar_state = calendar_state.clone();
                            let refresh = refresh.clone();
                            toast.connect_button_clicked(move |_| {
                                state.borrow().app_config.borrow_mut().show_calendar(&calendar_id);
                                persist(&state, &calendar_state);
                                refresh();
                            });
                        }
                        toast_overlay.add_toast(toast);
                    }
                }
                calendar_view::CalendarSidebarAction::MoveToGroup(calendar_id, group) => {
                    let previous = state.borrow().app_config.borrow().calendar_group_of(&calendar_id).map(|g| g.id.clone());
                    state.borrow().app_config.borrow_mut().move_calendar_to_group(&calendar_id, group.as_deref());
                    persist(&state, &calendar_state);
                    refresh();
                    let toast = adw::Toast::new("Calendar moved");
                    toast.set_button_label(Some("Undo"));
                    {
                        let state = state.clone();
                        let calendar_state = calendar_state.clone();
                        let refresh = refresh.clone();
                        toast.connect_button_clicked(move |_| {
                            state.borrow().app_config.borrow_mut().move_calendar_to_group(&calendar_id, previous.as_deref());
                            persist(&state, &calendar_state);
                            refresh();
                        });
                    }
                    toast_overlay.add_toast(toast);
                }
                calendar_view::CalendarSidebarAction::NewGroup => {
                    let state = state.clone();
                    let calendar_state = calendar_state.clone();
                    let refresh = refresh.clone();
                    crate::calendar_groups::show_name_dialog(&window, "New group", "Group name", "Create", "", move |name| {
                        let group = crate::app_config::CalendarGroup {
                            id: crate::app_config::AppConfig::new_calendar_group_id(),
                            name,
                            calendar_ids: Vec::new(),
                        };
                        state.borrow().app_config.borrow_mut().upsert_calendar_group(group);
                        persist(&state, &calendar_state);
                        refresh();
                    });
                }
                calendar_view::CalendarSidebarAction::RenameGroup(group_id) => {
                    let initial = state
                        .borrow()
                        .app_config
                        .borrow()
                        .calendar_groups
                        .iter()
                        .find(|g| g.id == group_id)
                        .map(|g| g.name.clone())
                        .unwrap_or_default();
                    let state = state.clone();
                    let calendar_state = calendar_state.clone();
                    let refresh = refresh.clone();
                    crate::calendar_groups::show_name_dialog(&window, "Rename group", "Group name", "Rename", &initial, move |name| {
                        state.borrow().app_config.borrow_mut().rename_calendar_group(&group_id, &name);
                        persist(&state, &calendar_state);
                        refresh();
                    });
                }
                calendar_view::CalendarSidebarAction::DeleteGroup(group_id) => {
                    let removed = {
                        let app_config = state.borrow().app_config.clone();
                        let mut config = app_config.borrow_mut();
                        let removed = config.calendar_groups.iter().find(|g| g.id == group_id).cloned();
                        config.remove_calendar_group(&group_id);
                        removed
                    };
                    persist(&state, &calendar_state);
                    refresh();
                    if let Some(group) = removed {
                        let toast = adw::Toast::new(&format!("Group \"{}\" deleted", group.name));
                        toast.set_button_label(Some("Undo"));
                        {
                            let state = state.clone();
                            let calendar_state = calendar_state.clone();
                            let refresh = refresh.clone();
                            toast.connect_button_clicked(move |_| {
                                state.borrow().app_config.borrow_mut().upsert_calendar_group(group.clone());
                                persist(&state, &calendar_state);
                                refresh();
                            });
                        }
                        toast_overlay.add_toast(toast);
                    }
                }
                calendar_view::CalendarSidebarAction::ShowAll => {
                    state.borrow().app_config.borrow_mut().show_all_calendars();
                    persist(&state, &calendar_state);
                    refresh();
                }
            }
        }
    });
    calendar_state.borrow_mut().calendar_sidebar_action = Some(handler);
}

/// Unions every connected calendar account's latest occurrences for
/// whatever month is currently displayed - filtered to only the calendars
/// currently checked in the sidebar - and redraws the whole main panel (every
/// view in the stack gets the merged set; each derives its own window from
/// the anchor). Same "only apply if it matches what's on screen" + "merge all
/// accounts' latest snapshot" approach as Mail's
/// `MessagesUpdated`/`rebuild_folder_tree`.
pub(crate) fn refresh_displayed_calendar_view(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_main: &CalendarMain) {
    let mut st = calendar_state.borrow_mut();
    let month = st.displayed_month;
    let mut merged: Vec<EventOccurrence> = st.checked_occurrences(month).into_iter().cloned().collect();
    apply_pending_calendar_moves(&mut merged, &mut st.pending_calendar_moves);
    drop(st);
    calendar_view::set_occurrences(calendar_main, &merged);
}

/// Overwrites each occurrence in `occurrences` with its pending drag-move's
/// `start`/`end`, if one exists for its `(uid, recurrence_id)` - unless the
/// occurrence's own start/end already match the pending value, meaning the
/// server has now confirmed the move, in which case the entry is dropped
/// from `pending` instead (self-clearing).
pub(crate) fn apply_pending_calendar_moves(occurrences: &mut [EventOccurrence], pending: &mut HashMap<(EventUid, Option<chrono::DateTime<chrono::Utc>>), EventOccurrence>) {
    if pending.is_empty() {
        return;
    }
    let mut confirmed = Vec::new();
    for occ in occurrences.iter_mut() {
        let key = (occ.uid.clone(), occ.recurrence_id);
        if let Some(patch) = pending.get(&key) {
            if occ.start == patch.start && occ.end == patch.end {
                confirmed.push(key);
            } else {
                occ.start = patch.start;
                occ.end = patch.end;
            }
        }
    }
    for key in confirmed {
        pending.remove(&key);
    }
}

/// Prints the currently-displayed calendar month as an agenda: one section
/// per day, each day's events sorted by start time (all-day first), with
/// calendar names as context. Uses the same merged occurrence set as
/// [`refresh_displayed_calendar_view`] - checked calendars only, whatever
/// month the user is looking at - so the printout matches the view.
pub(crate) fn print_calendar_month<T: IsA<gtk::Window>>(calendar_state: &Rc<RefCell<CalendarUiState>>, window: &T) {
    let st = calendar_state.borrow();
    let month = st.displayed_month;
    let merged: Vec<EventOccurrence> = st.checked_occurrences(month).into_iter().cloned().collect();
    let mut calendar_names: std::collections::HashMap<CalendarId, String> = std::collections::HashMap::new();
    for handle in st.accounts.values() {
        for calendar in &handle.calendars {
            calendar_names.insert(calendar.id.clone(), calendar.display_name.clone());
        }
    }
    for handle in st.webcal_handles.values() {
        calendar_names.insert(handle.calendar_id.clone(), handle.display_name.clone());
    }
    if let Some(birthdays) = &st.birthdays {
        calendar_names.insert(birthdays.calendar_id.clone(), birthdays.display_name.clone());
    }
    let month_start = month.with_day(1).unwrap();
    let month_end = (month_start + chrono::Months::new(1)) - chrono::Duration::days(1);
    let mut by_day: std::collections::BTreeMap<chrono::NaiveDate, Vec<EventOccurrence>> = std::collections::BTreeMap::new();
    for occ in &merged {
        for day in calendar_view::covered_local_dates(occ, month_start, month_end) {
            by_day.entry(day).or_default().push(occ.clone());
        }
    }
    drop(st);
    let esc = |s: &str| gtk::glib::markup_escape_text(s).to_string();
    let mut sections = String::new();
    let mut day = month_start;
    while day <= month_end {
        if let Some(events) = by_day.get(&day) {
            let mut sorted = events.clone();
            sorted.sort_by_key(|occ| (occ.all_day, occ.start));
            let mut items = String::new();
            for occ in sorted {
                let time = if occ.all_day {
                    "All day".to_string()
                } else {
                    let (start, end) = calendar_view::local_span(&occ);
                    format!("{} – {}", start.format("%H:%M"), end.format("%H:%M"))
                };
                let summary = occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string());
                let calendar = calendar_names.get(&occ.calendar_id).cloned().unwrap_or_default();
                items.push_str(&format!(
                    "<li><span class=\"time\">{}</span><span class=\"title\">{}</span>{}</li>",
                    esc(&time),
                    esc(&summary),
                    if calendar.is_empty() {
                        String::new()
                    } else {
                        format!(" <span class=\"cal\">· {}</span>", esc(&calendar))
                    },
                ));
            }
            sections.push_str(&format!("<h2>{}</h2><ul>{}</ul>", esc(&day.format("%A, %B %-e, %Y").to_string()), items));
        }
        day += chrono::Duration::days(1);
    }
    let html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <style>body {{ font-family: sans-serif; color: #1f2328; margin: 2em; }} \
         h1 {{ font-size: 1.5em; margin: 0 0 0.6em; }} \
         h2 {{ font-size: 1.1em; margin: 1.1em 0 0.3em; border-bottom: 1px solid #d0d7de; padding-bottom: 0.15em; }} \
         ul {{ list-style: none; margin: 0; padding: 0; }} \
         li {{ padding: 0.15em 0; }} \
         .time {{ display: inline-block; min-width: 9em; color: #57606a; font-variant-numeric: tabular-nums; }} \
         .title {{ font-weight: 600; }} \
         .cal {{ color: #57606a; }}</style>\
         </head><body><h1>{}</h1>{}</body></html>",
        esc(&month_start.format("%B %Y").to_string()),
        sections,
    );
    print_html_once(&html, window);
}

/// Fills the Mail-screen overview pane's day list with two sections: every
/// checked calendar's occurrences (from whatever's currently cached - no new
/// fetch here) whose local date matches `day`, sorted by start time, and
/// every uncompleted task from every source (CalDAV, Google Tasks, local) -
/// tasks aren't day-scoped, since most have no due date at all, so the whole
/// outstanding set appears regardless of which day the mini-calendar shows,
/// in the same bucket order the Lookout dashboard's task section uses. The
/// event section is headed by `day`'s date ("Today"/"Tomorrow"/"Tue 12 Aug")
/// and shows a "No events" placeholder when there are none; the task section
/// shows a "No outstanding tasks" placeholder when the set is empty. Task
/// rows are the Tasks view's own rows, captioned to match the event list
/// and without the completion checkbox - clicking a row opens the shared
/// task editor - with the click handler `build_window` registered on
/// `calendar_state`. Unlike
/// `refresh_displayed_calendar_view`, the event filter is by exact day
/// rather than by the main Calendar view's own displayed month - the
/// overview pane can be showing a day from a different month entirely.
/// How many rows the mail overview panel's "Upcoming events" section shows.
pub(crate) const MAIL_UPCOMING_EVENTS_LIMIT: usize = 10;

/// How far ahead that section looks (bounded by what the sessions have
/// synced; `widen_calendar_sync_horizon` fetches the months covering it).
pub(crate) const MAIL_UPCOMING_HORIZON_DAYS: i64 = 90;

pub(crate) fn refresh_mail_overview_day_list(calendar_state: &Rc<RefCell<CalendarUiState>>, day: chrono::NaiveDate, day_list_box: &gtk::Box) {
    while let Some(child) = day_list_box.first_child() {
        day_list_box.remove(&child);
    }

    let (occurrences, tasks, colors, checked, activate, upcoming_sources) = {
        let st = calendar_state.borrow();
        let occurrences: Vec<EventOccurrence> = st
            .checked_occurrences_all_months()
            .into_iter()
            .filter(|occ| !calendar_view::covered_local_dates(occ, day, day).is_empty())
            .cloned()
            .collect();
        let mut occurrences = occurrences;
        occurrences.sort_by_key(|occ| occ.start);
        // The `merged_tasks` union, inlined here so everything reads from
        // one borrow of `calendar_state`.
        let tasks: Vec<CalendarTask> = st
            .accounts
            .values()
            .flat_map(|h| h.last_tasks.iter().cloned())
            .chain(st.google_tasks.values().flat_map(|h| h.last_tasks.iter().cloned()))
            .chain(st.local_tasks.iter().cloned())
            .collect();
        let colors = st.calendar_colors.clone();
        let checked = st.checked_calendar_ids.clone();
        let activate = st.mail_overview_activate.clone().unwrap_or_else(|| Rc::new(|_t| {}));
        // The "Upcoming events" section reads the per-month caches
        // (`occurrences_by_month`) rather than `last_occurrences`, which
        // only holds the last-synced month - the caches span the 90-day
        // window `widen_calendar_sync_horizon` keeps fetched.
        let upcoming_sources: Vec<EventOccurrence> = st
            .accounts
            .values()
            .flat_map(|h| h.occurrences_by_month.values().flatten().cloned())
            .chain(st.webcal_handles.values().flat_map(|h| h.occurrences_by_month.values().flatten().cloned()))
            .chain(st.birthdays.as_ref().into_iter().flat_map(|h| h.occurrences_by_month.values().flatten().cloned()))
            .collect();
        (occurrences, tasks, colors, checked, activate, upcoming_sources)
    };

    let now = crate::calendar_tz::display_zone().now();
    let mut upcoming: Vec<EventOccurrence> = crate::lookout_view::upcoming_occurrences(
        upcoming_sources.iter(),
        now,
        chrono::Duration::days(MAIL_UPCOMING_HORIZON_DAYS),
        &checked,
        MAIL_UPCOMING_EVENTS_LIMIT,
    )
    .into_iter()
    .cloned()
    .collect();
    // The selected day's events are already listed in the section above;
    // keep only the ones that come after it.
    upcoming.retain(|occ| calendar_view::covered_local_dates(occ, day, day).is_empty());

    let header = gtk::Label::builder()
        .label(calendar_view::agenda_day_header(day, now.date()))
        .css_classes(["caption-heading"])
        .xalign(0.0)
        .build();
    crate::privacy::mark_ui(&header);
    day_list_box.append(&header);

    if occurrences.is_empty() {
        let placeholder = gtk::Label::builder().label("No events").css_classes(["dim-label", "caption"]).xalign(0.0).build();
        crate::privacy::mark_ui(&placeholder);
        day_list_box.append(&placeholder);
    } else {
        // A grid rather than one label per row: it auto-sizes the prefix
        // column to fit "All Day" (wider than any "HH:MM"), so every row's
        // title starts at the same x regardless of which prefix it has.
        let grid = gtk::Grid::builder().row_spacing(10).column_spacing(6).build();
        for (row, occ) in occurrences.into_iter().enumerate() {
            let prefix_text = if occ.all_day {
                "All Day".to_string()
            } else {
                calendar_view::local_span(&occ).0.format("%H:%M").to_string()
            };
            append_overview_event_row(&grid, row as i32, &prefix_text, &occ, &colors);
        }
        day_list_box.append(&grid);
    }

    let upcoming_header = gtk::Label::builder()
        .label("Upcoming events")
        .css_classes(["caption-heading"])
        .xalign(0.0)
        .margin_top(8)
        .build();
    crate::privacy::mark_ui(&upcoming_header);
    day_list_box.append(&upcoming_header);

    if upcoming.is_empty() {
        let placeholder = gtk::Label::builder().label("No upcoming events").css_classes(["dim-label", "caption"]).xalign(0.0).build();
        crate::privacy::mark_ui(&placeholder);
        day_list_box.append(&placeholder);
    } else {
        let grid = gtk::Grid::builder().row_spacing(10).column_spacing(6).build();
        for (row, occ) in upcoming.into_iter().enumerate() {
            let start = calendar_view::local_span(&occ).0;
            let today = now.date();
            let date = start.date();
            let prefix_text = if date == today {
                "Today".to_string()
            } else if date == today + chrono::Days::new(1) {
                "Tomorrow".to_string()
            } else {
                start.format("%a %d %b").to_string()
            };
            append_overview_event_row(&grid, row as i32, &prefix_text, &occ, &colors);
        }
        day_list_box.append(&grid);
    }

    let tasks_header = gtk::Label::builder().label("To Do").css_classes(["caption-heading"]).xalign(0.0).margin_top(8).build();
    crate::privacy::mark_ui(&tasks_header);
    day_list_box.append(&tasks_header);

    let outstanding = crate::lookout_view::outstanding_tasks(&tasks, chrono::Local::now().naive_local(), usize::MAX);
    if outstanding.is_empty() {
        let placeholder = gtk::Label::builder()
            .label("No outstanding tasks")
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .build();
        crate::privacy::mark_ui(&placeholder);
        day_list_box.append(&placeholder);
    } else {
        for task in outstanding {
            day_list_box.append(&crate::tasks_view::task_row(
                &task,
                &colors,
                Rc::new(|_t, _u, _c| {}),
                activate.clone(),
                &["caption"],
                false,
                &[],
            ));
        }
    }
    // Open follow-up flags (item 1.16), as the Tasks view lists them.
    let today = crate::calendar_tz::display_zone().today();
    let flagged = crate::tasks_view::flagged_mail_rows(&calendar_state.borrow().flagged_mail, today, crate::message_list::current_week_start());
    if !flagged.is_empty() {
        let flagged_header = gtk::Label::builder()
            .label("Flagged email")
            .css_classes(["caption-heading"])
            .xalign(0.0)
            .margin_top(8)
            .build();
        crate::privacy::mark_ui(&flagged_header);
        day_list_box.append(&flagged_header);
        let open = crate::tasks_view::open_message_through_app();
        for message in &flagged {
            day_list_box.append(&crate::tasks_view::flagged_mail_row(message, today, None, open.clone(), &["caption"]));
        }
    }
}

/// One event row in the mail overview panel's lists (the selected day's
/// events and the upcoming section): a colour dot, a prefix label (time,
/// date, or "All Day"), and the event title, laid out in a shared grid so
/// all titles align across rows.
pub(crate) fn append_overview_event_row(grid: &gtk::Grid, row: i32, prefix_text: &str, occ: &EventOccurrence, colors: &calendar_colors::CalendarColorMap) {
    let color = colors.get(&occ.calendar_id).map(String::as_str).unwrap_or(calendar_colors::DEFAULT_CHECK_COLOR).to_string();
    let dot = gtk::DrawingArea::builder().width_request(8).height_request(8).valign(gtk::Align::Center).build();
    dot.set_draw_func(move |_, cr, width, height| {
        let (r, g, b) = calendar_colors::rgb(&color);
        let radius = width.min(height) as f64 / 2.0 - 1.0;
        cr.arc(width as f64 / 2.0, height as f64 / 2.0, radius, 0.0, 2.0 * std::f64::consts::PI);
        cr.set_source_rgba(r, g, b, 1.0);
        let _ = cr.fill();
    });
    dot.set_tooltip_text(Some(&occ.calendar_id.0));
    let prefix = gtk::Label::builder().label(prefix_text).xalign(0.0).css_classes(["dim-label", "caption"]).build();
    let title = gtk::Label::builder()
        .label(occ.summary.as_deref().unwrap_or("(untitled)"))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption"])
        .build();
    grid.attach(&dot, 0, row, 1, 1);
    grid.attach(&prefix, 1, row, 1, 1);
    grid.attach(&title, 2, row, 1, 1);
}

pub(crate) fn calendar_account_label(state: &Rc<RefCell<CalendarUiState>>, account_id: &AccountId) -> String {
    state
        .borrow()
        .accounts
        .get(account_id)
        .map(|h| h.display_name.clone())
        .unwrap_or_else(|| account_id.0.clone())
}

/// The first calendar the user can share - an own (non-delegated) calendar of
/// a connected Microsoft 365 account (item 5.6 / CAL-24). `None` when the
/// account has no Graph calendars (so the caller explains that sharing is a
/// Microsoft 365 feature).
pub(crate) fn first_shareable_calendar(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Option<CalendarInfo> {
    let st = calendar_state.borrow();
    let mut handles: Vec<&CalendarAccountHandle> = st.accounts.values().filter(|h| h.graph).collect();
    handles.sort_by_key(|h| h.display_name.to_lowercase());
    handles.into_iter().find_map(|h| h.calendars.iter().find(|c| c.owner.is_none()).cloned())
}

/// The `(label, id)` list the event editor's calendar picker shows: every
/// discovered calendar across every connected account, labelled
/// "account · calendar" and sorted by account name.
pub(crate) fn pickable_calendars(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Vec<(String, CalendarId)> {
    let st = calendar_state.borrow();
    let mut handles: Vec<&CalendarAccountHandle> = st.accounts.values().collect();
    handles.sort_by_key(|h| h.display_name.to_lowercase());
    let mut out = Vec::new();
    for handle in handles {
        for calendar in &handle.calendars {
            out.push((format!("{} · {}", handle.display_name, calendar.display_name), calendar.id.clone()));
        }
    }
    out
}

/// The `(label, id)` list the task editor's calendar picker shows: like
/// [`pickable_calendars`], but restricted to sources that can actually hold
/// tasks - CalDAV calendars whose server advertises `VTODO` support
/// (Google's CalDAV is `VEVENT`-only and rejects task PUTs with HTTP 403),
/// plus every connected Google Tasks list ("<email> · <list>").
pub(crate) fn pickable_task_calendars(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Vec<(String, CalendarId)> {
    let st = calendar_state.borrow();
    let mut handles: Vec<&CalendarAccountHandle> = st.accounts.values().collect();
    handles.sort_by_key(|h| h.display_name.to_lowercase());
    let mut out = Vec::new();
    for handle in handles {
        for calendar in &handle.calendars {
            if calendar.supports_tasks {
                out.push((format!("{} · {}", handle.display_name, calendar.display_name), calendar.id.clone()));
            }
        }
    }
    for handle in st.google_tasks.values() {
        for list in &handle.task_lists {
            out.push((format!("{} · {}", handle.email, list.title), google_tasks::google_task_calendar_id(&list.id)));
        }
    }
    let mut graph_handles: Vec<&GraphTodoHandle> = st.graph_todo.values().collect();
    graph_handles.sort_by_key(|h| h.display_name.to_lowercase());
    for handle in graph_handles {
        for list in &handle.task_lists {
            out.push((format!("{} · {}", handle.display_name, list.display_name), lookout_graph::graph_todo_calendar_id(&list.id)));
        }
    }
    // The on-device lists are always available, so "New task" always has a
    // target wherever the user is (item 4.8 / T-01).
    for list in &st.local_task_lists {
        out.push((format!("Local · {}", list.name), local_task_list_calendar_id(&list.id)));
    }
    out
}

/// The task editor's default target: the first checked CalDAV calendar that
/// supports tasks, else any task-capable CalDAV calendar, else the first
/// Google Tasks list.
pub(crate) fn default_pickable_task_calendar(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Option<CalendarId> {
    let st = calendar_state.borrow();
    let task_capable = |calendar: &CalendarInfo| calendar.supports_tasks;
    for handle in st.accounts.values() {
        for calendar in &handle.calendars {
            if task_capable(calendar) && st.checked_calendar_ids.contains(&calendar.id) {
                return Some(calendar.id.clone());
            }
        }
    }
    if let Some(calendar) = st.accounts.values().find_map(|handle| handle.calendars.iter().find(|c| task_capable(c))) {
        return Some(calendar.id.clone());
    }
    if let Some(list) = st.google_tasks.values().find_map(|handle| handle.task_lists.first()) {
        return Some(google_tasks::google_task_calendar_id(&list.id));
    }
    st.graph_todo
        .values()
        .find_map(|handle| handle.task_lists.first().map(|l| lookout_graph::graph_todo_calendar_id(&l.id)))
}

/// The "Add as Task" mail toolbar button's default target: the preferred
/// account's own first task-capable CalDAV calendar (looked up by
/// `AccountId`, the id space `CalendarUiState::accounts` shares with a
/// GOA-connected mail account) or Google Tasks list (looked up by email,
/// the id `CalendarUiState::google_tasks` is keyed by since a Google Tasks
/// connection isn't necessarily tied to a GOA calendar account) - so a task
/// created from an email defaults into that email's own account rather than
/// an arbitrary one. Falls back to `default_pickable_task_calendar`'s
/// ordering, then finally the local on-device list.
pub(crate) fn default_task_calendar_preferring(calendar_state: &Rc<RefCell<CalendarUiState>>, preferred_account: Option<&AccountId>, preferred_email: Option<&str>) -> CalendarId {
    let st = calendar_state.borrow();
    if let Some(preferred_account) = preferred_account {
        if let Some(calendar) = st.accounts.get(preferred_account).and_then(|h| h.calendars.iter().find(|c| c.supports_tasks)) {
            return calendar.id.clone();
        }
        if let Some(list) = st.graph_todo.get(preferred_account).and_then(|h| h.task_lists.first()) {
            return lookout_graph::graph_todo_calendar_id(&list.id);
        }
    }
    if let Some(preferred_email) = preferred_email {
        if let Some(list) = st.google_tasks.get(preferred_email).and_then(|h| h.task_lists.first()) {
            return google_tasks::google_task_calendar_id(&list.id);
        }
    }
    drop(st);
    default_pickable_task_calendar(calendar_state).unwrap_or_else(local_tasks_calendar_id)
}

/// The editor's default calendar for a new event: the first checked calendar
/// (the one whose events are actually on screen), or any calendar if nothing
/// is checked.
pub(crate) fn default_pickable_calendar(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Option<CalendarId> {
    let st = calendar_state.borrow();
    for handle in st.accounts.values() {
        for calendar in &handle.calendars {
            if st.checked_calendar_ids.contains(&calendar.id) {
                return Some(calendar.id.clone());
            }
        }
    }
    st.accounts.values().find_map(|handle| handle.calendars.first().map(|c| c.id.clone()))
}

/// Best-effort address for the account owning `calendar_id`, used as
/// `ORGANIZER` when an event ends up with attendees. Prefers the connected
/// mail account behind the same GOA account (the identity invitations
/// actually send from, see `invite_identity`); GOA calendar accounts carry no
/// dedicated email field, so the display name - commonly *is* the account's
/// address (Google/Nextcloud/Fastmail all populate it that way) - is the
/// fallback, accepted only when it actually looks like one, else `None` (a
/// documented non-conformance the event editor already tolerates).
pub(crate) fn calendar_owner_email(state: &Rc<RefCell<UiState>>, calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> Option<String> {
    if let Some((identity, _)) = invite_identity(state, calendar_state, calendar_id) {
        return Some(identity.email);
    }
    let st = calendar_state.borrow();
    let display_name = st
        .accounts
        .values()
        .find(|handle| handle.calendars.iter().any(|c| c.id == *calendar_id))?
        .display_name
        .clone();
    crate::recipient_entry::is_plausible_address(&display_name).then_some(display_name)
}

/// A read-only snapshot for the event editor's right-hand preview panel:
/// every occurrence from checked calendars in the month containing `anchor`,
/// which local dates in that month have events, and the calendar-color map -
/// the same filtering `calendar_event_days`/`refresh_mail_overview_day_list`
/// already do for the sidebar's own mini-calendar. The editor never fetches
/// anything itself, so this is exactly what it can show.
pub(crate) struct EventEditorPreviewData {
    pub(crate) occurrences: Vec<EventOccurrence>,
    pub(crate) month_event_days: HashSet<chrono::NaiveDate>,
    pub(crate) colors: calendar_colors::CalendarColorMap,
}

pub(crate) fn event_editor_preview_data(calendar_state: &Rc<RefCell<CalendarUiState>>, anchor: chrono::NaiveDate) -> EventEditorPreviewData {
    let occurrences: Vec<EventOccurrence> = {
        let st = calendar_state.borrow();
        st.checked_occurrences_all_months().into_iter().cloned().collect()
    };
    let month_event_days = calendar_event_days(calendar_state, first_of_month(anchor));
    let colors = calendar_state.borrow().calendar_colors.clone();
    EventEditorPreviewData {
        occurrences,
        month_event_days,
        colors,
    }
}

/// Opens the editor for an existing occurrence — the shared path behind
/// clicking an event chip in the calendar grids and the notification
/// reminder's Open action (the engine hands the full occurrence back, so the
/// editor works even when the month it belongs to isn't the one on screen).
///
/// An occurrence from a read-only synthetic calendar (a webcal subscription,
/// or the birthdays calendar) opens the editor in read-only mode - neither
/// has a write-back path - so every input is disabled and the save/delete
/// actions are hidden, with a dim note explaining the source.
pub(crate) fn open_event_editor_for(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    profile_card: &crate::profile_card::ContextProvider,
    occ: &EventOccurrence,
) {
    let (read_only, read_only_note) = {
        let st = calendar_state.borrow();
        let note = st.read_only_note(&occ.calendar_id);
        (note.is_some(), note.unwrap_or(""))
    };
    let calendars = pickable_calendars(calendar_state);
    let default_calendar = if read_only {
        occ.calendar_id.clone()
    } else {
        if calendars.is_empty() {
            return;
        }
        if calendars.iter().any(|(_, id)| *id == occ.calendar_id) {
            occ.calendar_id.clone()
        } else {
            default_pickable_calendar(calendar_state).unwrap_or_else(|| CalendarId(String::new()))
        }
    };
    let preview = event_editor_preview_data(calendar_state, calendar_view::local_span(occ).0.date());
    let owner_email = calendar_owner_email(state, calendar_state, &occ.calendar_id);
    let calendar_prefs = state.borrow().settings.calendar_preferences();
    let categories = state.borrow().app_config.borrow().categories.clone();
    let scheduling = scheduling_context(state, calendar_state);
    let rooms = room_finder_context(calendar_state);
    let graph_calendars = graph_calendar_ids(calendar_state);
    crate::event_editor::show_event_editor(
        window,
        crate::event_editor::EventEditorPrefill {
            calendars: &calendars,
            default_calendar,
            existing: Some(occ),
            suggested_start: None,
            suggested_end: None,
            suggested_subject: None,
            month_occurrences: &preview.occurrences,
            month_event_days: &preview.month_event_days,
            calendar_colors: &preview.colors,
            owner_email,
            read_only,
            read_only_note,
            calendar_prefs: &calendar_prefs,
            graph_calendars: &graph_calendars,
            categories: &categories,
            suggested_attendees: &[],
            scheduling: Some(&scheduling),
            attachment_fetch: Some(attachment_fetch_callback(calendar_state)),
            rooms: Some(&rooms),
        },
        calendar_attendee_suggestions(state, worker),
        profile_card.clone(),
        {
            let window = window.clone();
            let state = state.clone();
            let calendar_state = calendar_state.clone();
            let toast_overlay = toast_overlay.clone();
            let previous = occ.clone();
            move |calendar_id, event| {
                save_event_with_invites(&window, &state, &calendar_state, &toast_overlay, Some(&previous), calendar_id, event);
            }
        },
        {
            let window = window.clone();
            let state = state.clone();
            let calendar_state = calendar_state.clone();
            let toast_overlay = toast_overlay.clone();
            move |calendar_id, occ| {
                delete_event_with_invites(&window, &state, &calendar_state, &toast_overlay, calendar_id, occ);
            }
        },
        {
            let window = window.clone();
            let state = state.clone();
            let calendar_state = calendar_state.clone();
            let toast_overlay = toast_overlay.clone();
            let forward_occ = occ.clone();
            Some(Rc::new(move || {
                let Some((identity, cmd_tx)) = invite_identity(&state, &calendar_state, &forward_occ.calendar_id) else {
                    no_invite_sender_toast(&toast_overlay, "Meeting not forwarded");
                    return;
                };
                let event = crate::calendar_invites::stored_event_from_occurrence(&forward_occ);
                show_forward_meeting_dialog(window.upcast_ref(), &toast_overlay, &identity, &cmd_tx, &event);
            }) as crate::event_editor::ForwardCallback)
        },
    );
}

/// Shows the Month/Split event peek card (UX-56) anchored to `anchor` (the
/// clicked chip). Edit opens the full editor, Delete routes through the same
/// invite-aware delete path the editor uses, and Join opens the meeting URL.
/// Read-only (webcal) events get no Delete.
#[allow(clippy::too_many_arguments)]
pub(crate) fn show_event_peek(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    profile_card: &crate::profile_card::ContextProvider,
    occ: &EventOccurrence,
    anchor: &gtk::Widget,
) {
    let colors = calendar_state.borrow().calendar_colors.clone();
    let read_only = calendar_state.borrow().is_read_only_calendar(&occ.calendar_id);
    let on_edit: Rc<dyn Fn()> = {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let profile_card = profile_card.clone();
        let occ = occ.clone();
        Rc::new(move || {
            open_event_editor_for(&window, &state, &worker, &calendar_state, &toast_overlay, &profile_card, &occ);
        })
    };
    let on_delete: Option<Rc<dyn Fn()>> = (!read_only).then(|| {
        let window = window.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let occ = occ.clone();
        let callback: Rc<dyn Fn()> = Rc::new(move || {
            delete_event_with_invites(&window, &state, &calendar_state, &toast_overlay, occ.calendar_id.clone(), occ.clone());
        });
        callback
    });
    let popover = calendar_view::build_event_peek(occ, &colors, on_edit, on_delete);
    popover.set_parent(anchor);
    popover.popup();
}

/// A field change a context-menu action makes to an event (item 3.12 /
/// CAL-31), applied to a clone of the stored event before it's re-PUT.
#[derive(Clone)]
pub(crate) enum EventFieldEdit {
    ShowAs(lookout_core::interop::BusyStatus),
    /// `true` = Private, `false` = Public.
    Private(bool),
    Categories(Vec<String>),
}

impl EventFieldEdit {
    pub(crate) fn apply(&self, event: &mut CalendarEvent) {
        match self {
            EventFieldEdit::ShowAs(show_as) => event.show_as = *show_as,
            EventFieldEdit::Private(private) => {
                event.sensitivity = if *private {
                    lookout_core::EventSensitivity::Private
                } else {
                    lookout_core::EventSensitivity::Public
                };
            }
            EventFieldEdit::Categories(categories) => event.categories = categories.clone(),
        }
    }
}

/// Opens the event context menu (item 3.12 / CAL-31) on `anchor`, pointing at
/// `point` (in `anchor`'s coordinates) or at the whole widget.
#[allow(clippy::too_many_arguments)]
pub(crate) fn show_event_menu(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    profile_card: &crate::profile_card::ContextProvider,
    occ: &EventOccurrence,
    anchor: &gtk::Widget,
    point: Option<(f64, f64)>,
) {
    use crate::event_menu::{EventMenuState, NAMESPACE};

    let read_only = calendar_state.borrow().is_read_only_calendar(&occ.calendar_id);
    let has_writable_target = pickable_calendars(calendar_state).iter().any(|(_, id)| !calendar_state.borrow().is_read_only_calendar(id));
    let menu_state = EventMenuState {
        read_only,
        has_writable_target,
        is_recurring: occ.rrule.is_some() || occ.recurrence_id.is_some(),
        show_as: occ.show_as,
        private: occ.sensitivity == lookout_core::EventSensitivity::Private,
    };

    let description = crate::event_menu::event_menu_description(&menu_state);
    let dispatch: crate::context_menu::Dispatch = {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let profile_card = profile_card.clone();
        let occ = occ.clone();
        Rc::new(move |name, param| {
            let Some(action) = crate::event_menu::event_action_for(name, param.as_ref()) else {
                return;
            };
            run_event_menu_action(&window, &state, &worker, &calendar_state, &toast_overlay, &profile_card, &occ, action);
        })
    };
    let menu = crate::context_menu::ContextMenu::new(&description, &crate::event_menu::action_specs(), dispatch);
    for (name, enabled) in crate::event_menu::enabled_actions(&menu_state) {
        menu.set_enabled(name, enabled);
    }
    // Show as ▸ checks the event's current state; Private its sensitivity.
    menu.set_state("show-as", &occ.show_as.as_cdo().to_variant());
    menu.set_state("private", &menu_state.private.to_variant());
    let categorize = build_event_category_menu(window, state, calendar_state, toast_overlay, occ);
    menu.add_child(&categorize, crate::event_menu::CATEGORIZE_SLOT);

    let popover = menu.popover.clone();
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    popover.set_parent(anchor);
    menu.insert_action_group(anchor, NAMESPACE);
    if let Some((x, y)) = point {
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    }
    // Built per opening, so it's dropped once closed - after the activated
    // item has run, hence the idle (the message menu's idiom).
    popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
    });
    popover.popup();
}

/// The event menu's Categorize checklist (a custom slot, like the message
/// menu's): a check per master category, applied on toggle.
pub(crate) fn build_event_category_menu(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    occ: &EventOccurrence,
) -> gtk::Box {
    let master = state.borrow().app_config.borrow().categories.clone();
    let boxed = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    if master.is_empty() {
        let empty = gtk::Label::builder()
            .label("No categories defined yet")
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build();
        crate::privacy::mark_ui(&empty);
        boxed.append(&empty);
        return boxed;
    }
    for category in &master {
        let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        row.append(&preset_dot_area(category.color(), 10));
        let check = gtk::CheckButton::builder().label(&category.name).build();
        // A category name is content: blurred by privacy mode.
        crate::privacy::mark_content(&check);
        check.set_active(occ.categories.iter().any(|name| name.eq_ignore_ascii_case(&category.name)));
        {
            let window = window.clone();
            let state = state.clone();
            let calendar_state = calendar_state.clone();
            let toast_overlay = toast_overlay.clone();
            let occ = occ.clone();
            let name = category.name.clone();
            check.connect_toggled(move |check| {
                let mut categories = occ.categories.clone();
                if check.is_active() {
                    if !categories.iter().any(|existing| existing.eq_ignore_ascii_case(&name)) {
                        categories.push(name.clone());
                    }
                } else {
                    categories.retain(|existing| !existing.eq_ignore_ascii_case(&name));
                }
                apply_event_field_edit(&window, &state, &calendar_state, &toast_overlay, &occ, EventFieldEdit::Categories(categories));
            });
        }
        row.append(&check);
        boxed.append(&row);
    }
    boxed
}

/// Runs one event context-menu action (item 3.12 / CAL-31) through the same
/// helpers the editor and ribbon use, so every verb behaves - and prompts for
/// invitations - the same wherever it's invoked.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_event_menu_action(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    profile_card: &crate::profile_card::ContextProvider,
    occ: &EventOccurrence,
    action: crate::event_menu::EventMenuAction,
) {
    use crate::event_menu::EventMenuAction as A;
    match action {
        A::Open => open_event_editor_for(window, state, worker, calendar_state, toast_overlay, profile_card, occ),
        A::Delete => delete_event_with_invites(window, state, calendar_state, toast_overlay, occ.calendar_id.clone(), occ.clone()),
        A::Duplicate => create_event_copy(window, state, calendar_state, toast_overlay, occ, occ.calendar_id.clone(), None, true, "Event duplicated"),
        A::CopyToCalendar => {
            let targets: Vec<(CalendarId, String, usize)> = pickable_calendars(calendar_state)
                .into_iter()
                .filter(|(_, id)| !calendar_state.borrow().is_read_only_calendar(id))
                .map(|(label, id)| (id, label, 0))
                .collect();
            if targets.is_empty() {
                toast_overlay.add_toast(adw::Toast::new("No other calendar to copy into."));
                return;
            }
            let window_for_pick = window.clone();
            let state_for_pick = state.clone();
            let calendar_state_for_pick = calendar_state.clone();
            let toast_overlay_for_pick = toast_overlay.clone();
            let occ_for_pick = occ.clone();
            crate::folder_menu::present_folder_picker(
                window.upcast_ref(),
                "Copy to calendar",
                "Choose the calendar to copy this event into.",
                "Copy",
                targets,
                move |target| {
                    create_event_copy(
                        &window_for_pick,
                        &state_for_pick,
                        &calendar_state_for_pick,
                        &toast_overlay_for_pick,
                        &occ_for_pick,
                        target,
                        None,
                        true,
                        "Event copied",
                    );
                },
            );
        }
        A::SaveAsIcs => save_event_ics(window, toast_overlay, occ),
        A::ShowAs(show_as) => {
            apply_event_field_edit(window, state, calendar_state, toast_overlay, occ, EventFieldEdit::ShowAs(show_as));
        }
        A::Private => {
            let make_private = occ.sensitivity != lookout_core::EventSensitivity::Private;
            apply_event_field_edit(window, state, calendar_state, toast_overlay, occ, EventFieldEdit::Private(make_private));
        }
    }
}

/// Clones `source` into `target` as a brand-new event (fresh UID, no
/// href/etag, sequence reset, recurrence dropped so the copy is standalone),
/// optionally at a new `times` span. Routes through `save_event_with_invites`
/// with `previous = None`, so an organizer's meeting sends invitations exactly
/// as a new meeting from the editor would. With `open_editor`, the new
/// occurrence's editor opens once the server confirms it (`pending_open_editor`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_event_copy(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    source: &EventOccurrence,
    target: CalendarId,
    times: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    open_editor: bool,
    toast: &str,
) {
    if calendar_state.borrow().is_read_only_calendar(&target) {
        toast_overlay.add_toast(adw::Toast::new("That calendar is read-only."));
        return;
    }
    let (start, end) = times.unwrap_or((source.start, source.end));
    let (start_local, end_local) = crate::event_editor::dragged_local_span(source, start, end);
    let mut event = crate::event_editor::calendar_event_from_occurrence(source, start_local, end_local);
    let new_uid = EventUid(uuid::Uuid::new_v4().to_string());
    event.uid = new_uid.clone();
    event.calendar_id = target.clone();
    event.href = None;
    event.etag = None;
    event.sequence = 0;
    event.recurrence_id = None;
    event.recurrence_range = lookout_core::RecurrenceRange::default();
    event.rrule = None;
    event.exdates = Vec::new();
    event.rdates = Vec::new();
    if open_editor {
        calendar_state.borrow_mut().pending_open_editor = Some((target.clone(), new_uid));
    }
    save_event_with_invites(window, state, calendar_state, toast_overlay, None, target, event);
    toast_overlay.add_toast(adw::Toast::new(toast));
}

/// Applies a field change to an event (item 3.12 / CAL-31). A recurring event
/// asks for the edit scope first, exactly as the editor's own dropdown does,
/// so a single occurrence becomes an override rather than re-anchoring the
/// series.
pub(crate) fn apply_event_field_edit(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    occ: &EventOccurrence,
    edit: EventFieldEdit,
) {
    if calendar_state.borrow().is_read_only_calendar(&occ.calendar_id) {
        return;
    }
    if occ.rrule.is_none() && occ.recurrence_id.is_none() {
        let mut event = crate::calendar_invites::stored_event_from_occurrence(occ);
        edit.apply(&mut event);
        save_event_with_invites(window, state, calendar_state, toast_overlay, Some(occ), occ.calendar_id.clone(), event);
        return;
    }
    let window = window.clone();
    let parent = window.clone();
    let state = state.clone();
    let calendar_state = calendar_state.clone();
    let toast_overlay = toast_overlay.clone();
    let occ = occ.clone();
    present_event_edit_scope_dialog(&parent, move |scope| {
        let mut event = crate::calendar_invites::stored_event_from_occurrence(&occ);
        edit.apply(&mut event);
        crate::event_editor::apply_edit_scope(&mut event, scope, Some(&occ));
        save_event_with_invites(&window, &state, &calendar_state, &toast_overlay, Some(&occ), occ.calendar_id.clone(), event);
    });
}

/// Asks which part of a recurring series an inline edit applies to ("This
/// occurrence" / "This and following" / "All events"), matching the editor's
/// edit-scope dropdown. `on_choose` receives the scope index
/// [`crate::event_editor::apply_edit_scope`] expects.
pub(crate) fn present_event_edit_scope_dialog(parent: &adw::ApplicationWindow, on_choose: impl Fn(usize) + 'static) {
    let dialog = adw::AlertDialog::builder()
        .heading("Edit recurring event")
        .body("This event is part of a recurring series. Which events should the change apply to?")
        .default_response("this")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("this", "This occurrence");
    dialog.add_response("following", "This and following");
    dialog.add_response("all", "All events");
    dialog.set_response_appearance("this", adw::ResponseAppearance::Suggested);
    let on_choose: Rc<dyn Fn(usize)> = Rc::new(on_choose);
    dialog.connect_response(None, move |_, response| {
        let scope = match response {
            "this" => 0,
            "following" => 1,
            "all" => 2,
            _ => return,
        };
        on_choose(scope);
    });
    dialog.present(Some(parent));
}

/// Prompts for a location and writes `occ` (as its whole series when it's a
/// recurring event) to a `.ics` file, mirroring the message "Save as .eml…"
/// export. The serialized body is the same `build_vcalendar` a CalDAV PUT
/// sends, so the file is a valid RFC 5545 document.
pub(crate) fn save_event_ics(window: &adw::ApplicationWindow, toast_overlay: &adw::ToastOverlay, occ: &EventOccurrence) {
    let event = crate::calendar_invites::stored_event_from_occurrence(occ);
    let ics = lookout_dav::build_vcalendar(&event);
    let initial_name = event_ics_name(occ);
    let window = window.clone();
    let toast_overlay = toast_overlay.clone();
    glib::spawn_future_local(async move {
        let filter = gtk::FileFilter::new();
        filter.add_suffix("ics");
        filter.set_name(Some("iCalendar files (*.ics)"));
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Save event as .ics").initial_name(&initial_name).filters(&filters).build();
        let Ok(file) = dialog.save_future(Some(&window)).await else { return };
        let result = file
            .replace_contents_bytes_future(&glib::Bytes::from(ics.as_bytes()), None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
            .await;
        match result {
            Ok(_) => toast_overlay.add_toast(adw::Toast::new("Event saved as .ics")),
            Err(e) => {
                let title = glib::markup_escape_text(&format!("Couldn't save event: {e}"));
                toast_overlay.add_toast(adw::Toast::new(&title));
            }
        }
    });
}

/// A safe suggested filename for the `.ics` export dialog: the event's summary
/// plus `.ics`, or a plain placeholder when the summary is empty or contains
/// path separators (untrusted data must never reach the dialog as a path, the
/// same rationale as `eml_suggested_name`).
pub(crate) fn event_ics_name(occ: &EventOccurrence) -> String {
    let summary = occ.summary.as_deref().unwrap_or("").trim();
    if summary.is_empty() || summary.contains('/') || summary.contains('\\') {
        "event.ics".to_string()
    } else {
        format!("{summary}.ics")
    }
}

/// Opens the "New event" editor prefilled for `suggested_start` (and
/// optionally `suggested_end`), shared by the Calendar toolbar's New event
/// button, the Day/Week grids' highlighted slot ranges, and month-grid day
/// activation so every entry point behaves identically (a connect-a-calendar
/// toast when nothing writable exists, the editor otherwise).
#[allow(clippy::too_many_arguments)]
pub(crate) fn show_new_event_editor(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    suggested_start: chrono::NaiveDateTime,
    suggested_end: Option<chrono::NaiveDateTime>,
    attendees: &[EmailAddress],
    subject: Option<&str>,
    profile_card: &crate::profile_card::ContextProvider,
) {
    let calendars = pickable_calendars(calendar_state);
    let Some(default_calendar) = default_pickable_calendar(calendar_state) else {
        toast_overlay.add_toast(adw::Toast::new("Connect a calendar account to create events."));
        return;
    };
    let preview = event_editor_preview_data(calendar_state, suggested_start.date());
    let owner_email = calendar_owner_email(state, calendar_state, &default_calendar);
    let calendar_prefs = state.borrow().settings.calendar_preferences();
    let categories = state.borrow().app_config.borrow().categories.clone();
    let scheduling = scheduling_context(state, calendar_state);
    let rooms = room_finder_context(calendar_state);
    let graph_calendars = graph_calendar_ids(calendar_state);
    crate::event_editor::show_event_editor(
        window,
        crate::event_editor::EventEditorPrefill {
            calendars: &calendars,
            default_calendar,
            existing: None,
            suggested_start: Some(suggested_start),
            suggested_end,
            suggested_subject: subject,
            month_occurrences: &preview.occurrences,
            month_event_days: &preview.month_event_days,
            calendar_colors: &preview.colors,
            owner_email,
            read_only: false,
            read_only_note: "",
            calendar_prefs: &calendar_prefs,
            graph_calendars: &graph_calendars,
            categories: &categories,
            suggested_attendees: attendees,
            scheduling: Some(&scheduling),
            attachment_fetch: Some(attachment_fetch_callback(calendar_state)),
            rooms: Some(&rooms),
        },
        calendar_attendee_suggestions(state, worker),
        profile_card.clone(),
        {
            let window = window.clone();
            let state = state.clone();
            let calendar_state = calendar_state.clone();
            let toast_overlay = toast_overlay.clone();
            move |calendar_id, event| {
                save_event_with_invites(&window, &state, &calendar_state, &toast_overlay, None, calendar_id, event);
            }
        },
        {
            let window = window.clone();
            let state = state.clone();
            let calendar_state = calendar_state.clone();
            let toast_overlay = toast_overlay.clone();
            move |calendar_id, occ| {
                delete_event_with_invites(&window, &state, &calendar_state, &toast_overlay, calendar_id, occ);
            }
        },
        // A brand-new event isn't a received meeting - nothing to forward.
        None,
    );
}

/// The account session that owns `calendar_id`, so its command channel can
/// carry create/update/delete writes.
pub(crate) fn calendar_handle_for_id(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> Option<async_channel::Sender<CalendarCommand>> {
    calendar_state
        .borrow()
        .accounts
        .values()
        .find(|handle| handle.calendars.iter().any(|c| c.id == *calendar_id))
        .map(|handle| handle.cmd_tx.clone())
}

/// Asks the owning calendar session for one event attachment's bytes (item
/// 3.16 / CAL-21), running `on_success`/`on_error` when the answer arrives on
/// the calendar event loop. The UI never fetches a server URL itself: a CalDAV
/// managed attachment may need the account's credentials and a Graph child
/// resource is only reachable through the Graph actor.
pub(crate) fn request_event_attachment(
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    calendar_id: CalendarId,
    event_href: Option<String>,
    attachment: lookout_core::EventAttachment,
    on_success: impl Fn(String, Vec<u8>) + 'static,
    on_error: impl Fn(String) + 'static,
) {
    let Some(handle) = calendar_handle_for_id(calendar_state, &calendar_id) else {
        on_error(format!("the calendar holding \"{}\" isn't connected", attachment.name));
        return;
    };
    let request_id = {
        let mut state = calendar_state.borrow_mut();
        let request_id = state.attachment_next_id;
        state.attachment_next_id = state.attachment_next_id.wrapping_add(1);
        state.attachment_requests.insert(
            request_id,
            super::state::AttachmentRequest {
                on_success: Box::new(on_success),
                on_error: Box::new(on_error),
            },
        );
        request_id
    };
    let _ = handle.send_blocking(CalendarCommand::FetchEventAttachment {
        calendar_id,
        event_href,
        attachment,
        request_id,
    });
}

/// The event editor's attachment-save bridge (CAL-21), backed by
/// [`request_event_attachment`] over the window's calendar state.
pub(crate) fn attachment_fetch_callback(calendar_state: &Rc<RefCell<CalendarUiState>>) -> crate::event_editor::AttachmentFetchCallback {
    let calendar_state = calendar_state.clone();
    Rc::new(move |calendar_id, event_href, attachment, on_success, on_error| {
        request_event_attachment(
            &calendar_state,
            calendar_id,
            event_href,
            attachment,
            move |name, data| on_success(name, data),
            move |message| on_error(message),
        );
    })
}

/// Whether the calendar account owning `calendar_id` is served by the
/// Microsoft Graph backend (item 13): its invitation responses use Graph's own
/// RSVP endpoints instead of an iMIP REPLY.
pub(crate) fn calendar_account_is_graph(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> bool {
    calendar_state
        .borrow()
        .accounts
        .values()
        .any(|handle| handle.graph && handle.calendars.iter().any(|calendar| calendar.id == *calendar_id))
}

/// The calendar ids served by the Microsoft Graph backend. The event editor
/// uses this to refuse a recurrence pattern Graph's structured model can't
/// hold (item 3.10 / CAL-13) instead of letting the write drop it.
fn graph_calendar_ids(calendar_state: &Rc<RefCell<CalendarUiState>>) -> HashSet<CalendarId> {
    calendar_state
        .borrow()
        .accounts
        .values()
        .filter(|handle| handle.graph)
        .flat_map(|handle| handle.calendars.iter().map(|calendar| calendar.id.clone()))
        .collect()
}

/// Routes a saved (created or edited) event to its account's session. An event
/// with a `href` is an edit in place (`UpdateEvent`, guarded by its etag); one
/// without is a brand-new event (`CreateEvent`, fresh `<uid>.ics` href). The
/// session resyncs on success, which repaints the views.
pub(crate) fn route_calendar_save(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: CalendarId, event: CalendarEvent) {
    let Some(handle) = calendar_handle_for_id(calendar_state, &calendar_id) else {
        tracing::warn!("tried to save an event into an unknown calendar {calendar_id}");
        return;
    };
    if event.href.is_some() {
        let _ = handle.send_blocking(CalendarCommand::UpdateEvent { event: Box::new(event) });
    } else {
        let _ = handle.send_blocking(CalendarCommand::CreateEvent { event: Box::new(event) });
    }
}

/// Routes an event deletion to its account's session, honoring the event's
/// recurrence structure: an override (or a non-recurring event) deletes its
/// own resource; a plain expansion of a recurring series instead adds its
/// instance time to the master's EXDATEs (via the etag-guarded
/// `UpdateEvent`), removing exactly that instance without touching the rest
/// of the series.
pub(crate) fn route_calendar_delete(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: CalendarId, occ: EventOccurrence) {
    if calendar_state.borrow().is_read_only_calendar(&occ.calendar_id) {
        tracing::warn!("refusing to delete a read-only subscription event");
        return;
    }
    let Some(handle) = calendar_handle_for_id(calendar_state, &calendar_id) else {
        tracing::warn!("tried to delete an event from an unknown calendar {calendar_id}");
        return;
    };
    if occ.recurrence_id.is_some() {
        // A per-occurrence override: delete its own resource.
        let Some(href) = occ.href else {
            tracing::warn!("tried to delete an override event without a server href");
            return;
        };
        let _ = handle.send_blocking(CalendarCommand::DeleteEvent {
            calendar_id,
            href,
            etag: occ.etag,
        });
        return;
    }
    if occ.rrule.is_some() {
        // A plain instance of a recurring series: EXDATE it out of the
        // master. The occurrence already carries the master's fields -
        // its href/etag are the master's resource, and its exdates list is
        // the master's - so the only change is pushing the instance time.
        let mut master = CalendarEvent {
            uid: occ.uid.clone(),
            calendar_id: occ.calendar_id.clone(),
            summary: occ.summary.clone(),
            description: occ.description.clone(),
            location: occ.location.clone(),
            start: occ.start,
            end: occ.end,
            all_day: occ.all_day,
            time_zone: occ.time_zone.clone(),
            rrule: occ.rrule.clone(),
            recurrence_id: None,
            recurrence_range: lookout_core::RecurrenceRange::default(),
            exdates: occ.exdates.clone(),
            rdates: Vec::new(),
            href: occ.master_href.clone().or_else(|| occ.href.clone()),
            etag: occ.master_etag.clone().or_else(|| occ.etag.clone()),
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
        };
        master.exdates.push(occ.start);
        let _ = handle.send_blocking(CalendarCommand::UpdateEvent { event: Box::new(master) });
        return;
    }
    // A standalone non-recurring event: delete its resource outright.
    let Some(href) = occ.href else {
        tracing::warn!("tried to delete an event without a server href");
        return;
    };
    let _ = handle.send_blocking(CalendarCommand::DeleteEvent {
        calendar_id,
        href,
        etag: occ.etag,
    });
}

/// The owning account's discovered RFC 6638 scheduling capability, defaulting
/// to "the client schedules" for an unknown calendar (matching
/// `SchedulingInfo::default`) - the save/delete route warns about the unknown
/// calendar itself.
pub(crate) fn calendar_scheduling(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> SchedulingInfo {
    calendar_state
        .borrow()
        .accounts
        .values()
        .find(|handle| handle.calendars.iter().any(|calendar| calendar.id == *calendar_id))
        .map(|handle| handle.scheduling.clone())
        .unwrap_or_default()
}

/// The event editor's Scheduling Assistant bridge (item 3.8 / CAL-19): a
/// `SchedulingHandle` the account's event loop pushes the answer into, and a
/// request callback that routes a free/busy query to the owning account's
/// session - or reports a soft failure when that account's server doesn't
/// advertise scheduling (the editor then shows the attendees as unknown).
pub(crate) fn scheduling_context(state: &Rc<RefCell<UiState>>, calendar_state: &Rc<RefCell<CalendarUiState>>) -> crate::event_editor::SchedulingContext {
    let handle = crate::event_editor::SchedulingHandle::new();
    let request = {
        let handle = handle.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        Rc::new(move |request: crate::event_editor::AvailabilityRequest| {
            let (cmd_tx, supported) = {
                let st = calendar_state.borrow();
                match st.accounts.values().find(|h| h.calendars.iter().any(|c| c.id == request.calendar_id)) {
                    Some(account) => (Some(account.cmd_tx.clone()), account.scheduling.supports_server_scheduling()),
                    None => (None, false),
                }
            };
            if !supported {
                handle.fail(request.generation, "this calendar's server doesn't support scheduling".to_string());
                return;
            }
            let Some(cmd_tx) = cmd_tx else {
                handle.fail(request.generation, "no calendar account".to_string());
                return;
            };
            let Some(organizer) = calendar_owner_email(&state, &calendar_state, &request.calendar_id)
                .map(|email| email.trim().to_string())
                .filter(|email| !email.is_empty())
                .map(EmailAddress::new)
            else {
                handle.fail(request.generation, "no organizer address for this calendar".to_string());
                return;
            };
            let request_id = {
                let mut st = calendar_state.borrow_mut();
                st.scheduling_next_id += 1;
                st.scheduling_next_id
            };
            calendar_state.borrow_mut().scheduling_pending.insert(request_id, (handle.clone(), request.generation));
            let _ = cmd_tx.send_blocking(CalendarCommand::FetchFreeBusy {
                organizer,
                attendees: request.attendees,
                start: request.start,
                end: request.end,
                request_id,
            });
        }) as Rc<dyn Fn(crate::event_editor::AvailabilityRequest)>
    };
    crate::event_editor::SchedulingContext { handle, request }
}

/// The Microsoft 365 account whose calendar `calendar_id` belongs to, if that
/// account is served by the Graph backend. The room finder (CAL-19) only has a
/// directory to ask on such an account.
fn graph_calendar_account(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> Option<AccountId> {
    calendar_state
        .borrow()
        .accounts
        .iter()
        .find(|(_, handle)| handle.graph && handle.calendars.iter().any(|calendar| calendar.id == *calendar_id))
        .map(|(account_id, _)| account_id.clone())
}

/// Maps a Graph room-directory failure onto the popover's two states: a
/// missing/denied scope becomes the Allow prompt, anything else a visible
/// error. A tenant policy that blocks `Place.Read.All` still answers 403, so
/// the editor only offers Allow once before treating a repeat as policy.
fn room_lookup_error(error: lookout_graph::GraphError) -> crate::room_finder::RoomLookupError {
    use crate::room_finder::RoomLookupError;
    match error {
        lookout_graph::GraphError::ConsentRequired(_) | lookout_graph::GraphError::Unauthorized | lookout_graph::GraphError::Status { status: 401 | 403, .. } => {
            RoomLookupError::ConsentRequired
        }
        other => RoomLookupError::Unavailable(other.to_string()),
    }
}

/// The event editor's room-finder bridge (item 3.8 / CAL-19): the tenant room
/// directory and the candidate rooms' free/busy for the selected calendar's
/// Microsoft 365 account. CalDAV/webcal calendars have no room-directory
/// standard, so the editor never offers the action for them; every failure
/// degrades to the free-text Location rather than an error dialog (§10).
pub(crate) fn room_finder_context(calendar_state: &Rc<RefCell<CalendarUiState>>) -> crate::room_finder::RoomFinder {
    use crate::room_finder::{ConsentFinished, RoomAvailabilityLoaded, RoomAvailabilityLookup, RoomFinder, RoomLookupError, RoomSuggestion, RoomsLoaded};

    let load: Rc<dyn Fn(CalendarId, RoomsLoaded)> = {
        let calendar_state = calendar_state.clone();
        Rc::new(move |calendar_id, on_result: RoomsLoaded| {
            let Some(account_id) = graph_calendar_account(&calendar_state, &calendar_id) else {
                on_result(Err(RoomLookupError::Unavailable("This calendar isn't a Microsoft 365 calendar.".to_string())));
                return;
            };
            let client = crate::graph_rooms::GraphRoomsClient::new(account_id);
            glib::spawn_future_local(async move {
                match client.list_rooms().await {
                    Ok(rooms) => on_result(Ok(rooms.into_iter().map(RoomSuggestion::from).collect())),
                    Err(error) => on_result(Err(room_lookup_error(error))),
                }
            });
        })
    };

    let grant: Rc<dyn Fn(CalendarId, ConsentFinished)> = {
        let calendar_state = calendar_state.clone();
        Rc::new(move |calendar_id, on_result: ConsentFinished| {
            let Some(account_id) = graph_calendar_account(&calendar_state, &calendar_id) else {
                on_result(Err("This calendar isn't a Microsoft 365 calendar.".to_string()));
                return;
            };
            let client = crate::graph_rooms::GraphRoomsClient::new(account_id);
            glib::spawn_future_local(async move { on_result(client.ensure_consent().await) });
        })
    };

    let availability: RoomAvailabilityLookup = {
        let calendar_state = calendar_state.clone();
        Rc::new(move |calendar_id, addresses, start, end, generation, on_result: RoomAvailabilityLoaded| {
            let Some(account_id) = graph_calendar_account(&calendar_state, &calendar_id) else {
                on_result(generation, Vec::new());
                return;
            };
            let client = crate::graph_rooms::GraphRoomsClient::new(account_id);
            glib::spawn_future_local(async move {
                let answers = client.room_availability(&addresses, start, end).await;
                on_result(generation, answers);
            });
        })
    };

    RoomFinder { load, grant, availability }
}

/// The account session that owns `calendar_id`, for task writes - the
/// `calendar_handle_for_id` equivalent (same "calendar collection belongs to
/// its account's handle" lookup).
pub(crate) fn task_handle_for_id(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> Option<async_channel::Sender<CalendarCommand>> {
    calendar_state
        .borrow()
        .accounts
        .values()
        .find(|handle| handle.calendars.iter().any(|c| c.id == *calendar_id))
        .map(|handle| handle.cmd_tx.clone())
}

/// The synthetic calendar id of the on-device-only task store's built-in
/// default list (the fallback when no connected source supports tasks). Tasks
/// with this id live in the UI-state database and never sync anywhere.
pub(crate) fn local_tasks_calendar_id() -> CalendarId {
    CalendarId(crate::app_config::LOCAL_DEFAULT_LIST_ID.to_string())
}

/// The synthetic calendar id for a local task list with `id` (item 4.8 /
/// T-01): the bare `local` for the built-in default (matching the pre-multi-
/// list store), `local:<id>` for a user-created list - the same convention the
/// server-backed sources use.
pub(crate) fn local_task_list_calendar_id(id: &str) -> CalendarId {
    if id == crate::app_config::LOCAL_DEFAULT_LIST_ID {
        CalendarId(crate::app_config::LOCAL_DEFAULT_LIST_ID.to_string())
    } else {
        CalendarId(format!("{}:{id}", crate::app_config::LOCAL_DEFAULT_LIST_ID))
    }
}

/// The local task list id a synthetic calendar id names, if it names one.
pub(crate) fn local_task_list_id(calendar_id: &CalendarId) -> Option<&str> {
    let raw = calendar_id.0.as_str();
    if raw == crate::app_config::LOCAL_DEFAULT_LIST_ID {
        Some(crate::app_config::LOCAL_DEFAULT_LIST_ID)
    } else {
        raw.strip_prefix(&format!("{}:", crate::app_config::LOCAL_DEFAULT_LIST_ID))
    }
}

/// Whether `calendar_id` names any local (on-device) task list - the write
/// routing test, replacing the old single-list equality check.
pub(crate) fn is_local_task_calendar(calendar_id: &CalendarId) -> bool {
    local_task_list_id(calendar_id).is_some()
}

/// The Google Tasks session owning a `googletasks:<list id>` calendar, if
/// any - the task-write routing lookup.
pub(crate) fn google_tasks_handle_for_calendar<'a>(state: &'a CalendarUiState, calendar_id: &CalendarId) -> Option<&'a GoogleTasksHandle> {
    let list_id = google_tasks::google_task_list_id(calendar_id)?;
    state.google_tasks.values().find(|handle| handle.task_lists.iter().any(|l| l.id == list_id))
}

/// The Graph To Do session owning a `todo:<list id>` calendar, if any - the
/// task-write routing lookup (item 5.3 / A-05, T-12).
pub(crate) fn graph_todo_handle_for_calendar<'a>(state: &'a CalendarUiState, calendar_id: &CalendarId) -> Option<&'a GraphTodoHandle> {
    let list_id = lookout_graph::graph_todo_list_id(calendar_id)?;
    state.graph_todo.values().find(|handle| handle.task_lists.iter().any(|l| l.id == list_id))
}

/// Unions every connected task source's latest snapshot: CalDAV accounts'
/// tasks, Google Tasks accounts' tasks, and the local on-device store.
/// Tasks have no month window, so the full set is always the merge unit.
pub(crate) fn merged_tasks(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Vec<CalendarTask> {
    let st = calendar_state.borrow();
    st.accounts
        .values()
        .flat_map(|handle| handle.last_tasks.iter().cloned())
        .chain(st.google_tasks.values().flat_map(|handle| handle.last_tasks.iter().cloned()))
        .chain(st.graph_todo.values().flat_map(|handle| handle.last_tasks.iter().cloned()))
        .chain(st.local_tasks.iter().cloned())
        .collect()
}

/// The same merge as [`merged_tasks`], tagged with the account each task came
/// from - the shape the reminder engine's `sync_tasks` needs to prune a
/// disabled account's task alerts.
pub(crate) fn merged_tasks_with_accounts(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Vec<(AccountId, CalendarTask)> {
    let st = calendar_state.borrow();
    let mut out = Vec::new();
    for (account_id, handle) in &st.accounts {
        out.extend(handle.last_tasks.iter().cloned().map(|task| (account_id.clone(), task)));
    }
    for (email, handle) in &st.google_tasks {
        let account_id = AccountId(format!("googletasks:{email}"));
        out.extend(handle.last_tasks.iter().cloned().map(|task| (account_id.clone(), task)));
    }
    for (account_id, handle) in &st.graph_todo {
        out.extend(handle.last_tasks.iter().cloned().map(|task| (account_id.clone(), task)));
    }
    let local = AccountId("local".to_string());
    out.extend(st.local_tasks.iter().cloned().map(|task| (local.clone(), task)));
    out
}

/// Repaints the Tasks view from every source's latest snapshot, setting the
/// empty-state message to match what task sources exist (B: "you have no
/// tasks" vs "you have nowhere to put tasks").
pub(crate) fn refresh_tasks_view(calendar_state: &Rc<RefCell<CalendarUiState>>, tasks_view: &Rc<crate::tasks_view::TasksView>) {
    reload_my_day_if_needed(calendar_state);
    let colors = calendar_state.borrow().calendar_colors.clone();
    let tasks = merged_tasks(calendar_state);
    let list_names = task_list_names(calendar_state);
    let filter = calendar_state.borrow().tasks_filter.clone();
    let rows = task_nav_rows(calendar_state);
    let pins = calendar_state.borrow().my_day_pins.clone();

    let st = calendar_state.borrow();
    let has_caldav_tasks = st.accounts.values().flat_map(|h| h.calendars.iter()).any(|c| c.supports_tasks);
    let has_google_tasks = !st.google_tasks.is_empty();
    let has_graph_todo = !st.graph_todo.is_empty();
    let flagged = st.flagged_mail.clone();
    drop(st);
    let message = if has_caldav_tasks || has_google_tasks || has_graph_todo {
        "No tasks yet - use New task to create one.".to_string()
    } else {
        "No connected calendar supports tasks, so new tasks are saved on this device only. To sync tasks, use Connect Google Tasks or connect a calendar that supports them (Nextcloud, iCloud).".to_string()
    };
    crate::tasks_view::set_empty_message(tasks_view, &message);
    // The navigation pane's rows and the selected row (item 4.8 / T-01).
    crate::tasks_view::set_nav(tasks_view, &rows, &filter);
    // My Day's pin set (the Add/Remove handler was installed once by the
    // window); a no-op on any other row.
    crate::tasks_view::set_my_day_pins(tasks_view, &pins);
    // The main list renders `All`/`List` and clears itself for My Day/Flagged;
    // each dedicated setter then fills its own view.
    crate::tasks_view::set_tasks(tasks_view, &tasks, &colors, &list_names);
    let today = crate::calendar_tz::display_zone().today();
    let pinned = crate::my_day::pinned_tasks(&tasks, &pins);
    let suggestions = crate::my_day::suggestions(&tasks, &pins, today);
    crate::tasks_view::set_my_day(tasks_view, &pinned, &suggestions, &colors);
    crate::tasks_view::set_flagged_mail(tasks_view, &flagged);
    // Keep the reminder engine's task snapshot in step with the one the view
    // just painted (T-05). The hook is installed by the window once the engine
    // exists; before that this is a no-op.
    let task_reminders_refresh = calendar_state.borrow().task_reminders_refresh.clone();
    if let Some(hook) = task_reminders_refresh {
        hook(&merged_tasks_with_accounts(calendar_state));
    }
    // Every task change funnels through here (CalDAV and Google Tasks
    // `TasksUpdated` events, local saves, checkbox flips), so this is the
    // single point that keeps the Lookout dashboard's task section and the
    // Mail-screen overview pane's task list live.
    refresh_dashboard_hook(calendar_state);
    refresh_mail_overview_hook(calendar_state);
}

/// Reloads today's My Day pin set when the display-zone date has rolled over
/// since the mirror was last filled (item 4.9 / T-02) - a no-op on the same
/// day, so a repaint doesn't hit the database.
fn reload_my_day_if_needed(calendar_state: &Rc<RefCell<CalendarUiState>>) {
    let today = crate::calendar_tz::display_zone().today();
    if calendar_state.borrow().my_day_day == today {
        return;
    }
    let day = today.format("%Y-%m-%d").to_string();
    let pins = calendar_state
        .borrow()
        .local_tasks_db
        .clone()
        .and_then(|db| {
            db.prune_my_day(&day);
            db.reader().load_my_day(&day).ok()
        })
        .unwrap_or_default();
    let mut st = calendar_state.borrow_mut();
    st.my_day_day = today;
    st.my_day_pins = pins;
}

/// Every task list's id → display name, for the "group by list" mode (T-09)
/// and the navigation pane (item 4.8 / T-01): CalDAV calendars, Google Tasks
/// lists, To Do lists and the local lists.
pub(crate) fn task_list_names(calendar_state: &Rc<RefCell<CalendarUiState>>) -> HashMap<CalendarId, String> {
    let st = calendar_state.borrow();
    let mut names: HashMap<CalendarId, String> = st
        .accounts
        .values()
        .flat_map(|handle| handle.calendars.iter())
        .map(|cal| (cal.id.clone(), cal.display_name.clone()))
        .collect();
    for handle in st.google_tasks.values() {
        for list in &handle.task_lists {
            names.insert(google_tasks::google_task_calendar_id(&list.id), list.title.clone());
        }
    }
    for handle in st.graph_todo.values() {
        for list in &handle.task_lists {
            names.insert(lookout_graph::graph_todo_calendar_id(&list.id), list.display_name.clone());
        }
    }
    for list in &st.local_task_lists {
        names.insert(local_task_list_calendar_id(&list.id), list.name.clone());
    }
    names
}

/// The Tasks navigation pane's rows (item 4.8 / T-01, UX-10), in Outlook's
/// order: the smart lists (My Day, Important, Planned, Flagged email, Tasks),
/// then every source list - CalDAV task calendars, Google Tasks lists, To Do
/// lists and local lists - each with its assigned colour. The trailing
/// "+ New list" row is added by the view.
pub(crate) fn task_nav_rows(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Vec<crate::tasks_view::TasksNavRow> {
    use crate::tasks_view::{TasksFilter, TasksNavRow};
    let st = calendar_state.borrow();
    let colors = &st.calendar_colors;
    let dot = |id: &CalendarId| colors.get(id).cloned();
    let mut rows = vec![
        TasksNavRow {
            label: "My Day".to_string(),
            filter: TasksFilter::MyDay,
            color: None,
            list_id: None,
        },
        TasksNavRow {
            label: "Important".to_string(),
            filter: TasksFilter::Important,
            color: None,
            list_id: None,
        },
        TasksNavRow {
            label: "Planned".to_string(),
            filter: TasksFilter::Planned,
            color: None,
            list_id: None,
        },
        TasksNavRow {
            label: "Flagged email".to_string(),
            filter: TasksFilter::Flagged,
            color: None,
            list_id: None,
        },
        TasksNavRow {
            label: "Tasks".to_string(),
            filter: TasksFilter::All,
            color: None,
            list_id: None,
        },
    ];
    let mut handles: Vec<&CalendarAccountHandle> = st.accounts.values().collect();
    handles.sort_by_key(|h| h.display_name.to_lowercase());
    for handle in handles {
        for calendar in handle.calendars.iter().filter(|c| c.supports_tasks) {
            rows.push(TasksNavRow {
                label: calendar.display_name.clone(),
                filter: TasksFilter::List(calendar.id.clone()),
                color: dot(&calendar.id),
                list_id: Some(calendar.id.clone()),
            });
        }
    }
    for handle in st.google_tasks.values() {
        for list in &handle.task_lists {
            let id = google_tasks::google_task_calendar_id(&list.id);
            rows.push(TasksNavRow {
                label: list.title.clone(),
                filter: TasksFilter::List(id.clone()),
                color: dot(&id),
                list_id: Some(id),
            });
        }
    }
    let mut graph: Vec<&GraphTodoHandle> = st.graph_todo.values().collect();
    graph.sort_by_key(|h| h.display_name.to_lowercase());
    for handle in graph {
        for list in &handle.task_lists {
            // The well-known flagged-email list is Outlook's own view of
            // flagged mail (already the "Flagged email" smart list); skip it.
            if list.well_known.as_deref() == Some("flaggedEmails") || list.id == lookout_graph::WELL_KNOWN_FLAGGED_EMAIL {
                continue;
            }
            let id = lookout_graph::graph_todo_calendar_id(&list.id);
            rows.push(TasksNavRow {
                label: list.display_name.clone(),
                filter: TasksFilter::List(id.clone()),
                color: dot(&id),
                list_id: Some(id),
            });
        }
    }
    for list in &st.local_task_lists {
        let id = local_task_list_calendar_id(&list.id);
        rows.push(TasksNavRow {
            label: format!("{} (Local)", list.name),
            filter: TasksFilter::List(id.clone()),
            color: dot(&id),
            list_id: Some(id),
        });
    }
    rows
}

/// Runs the Lookout dashboard's repaint hook if the window has registered
/// one - a no-op in tests and before the window finishes wiring, never an
/// error.
pub(crate) fn refresh_dashboard_hook(calendar_state: &Rc<RefCell<CalendarUiState>>) {
    let hook = calendar_state.borrow().dashboard_refresh.clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Runs the Mail-screen overview pane's repaint hook if the window has
/// registered one - same no-op-until-wired convention as
/// `refresh_dashboard_hook`.
pub(crate) fn refresh_mail_overview_hook(calendar_state: &Rc<RefCell<CalendarUiState>>) {
    let hook = calendar_state.borrow().mail_overview_refresh.clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// One off-thread dashboard cache read's reply: the account's top contacts
/// and its all-time hour-of-day histogram, fetched in a single blocking hop.
pub(crate) type DashboardCacheReply = (Vec<(EmailAddress, i64)>, [i64; 24]);

/// Repaints the Lookout dashboard from the current mail caches and
/// calendar state: most-contacted people and the hour-of-day histogram are
/// read straight off each connected account's cache SQLite (the same WAL
/// reader the composer autocomplete uses, so no session round trip), and
/// tasks/events come from the in-memory calendar state.
///
/// The per-account cache reads run on the `Worker`'s blocking pool rather
/// than the GTK thread - the histogram in particular scans the whole
/// `messages` table ("all time", so no index can help it) and would block
/// the main loop for the scan's duration at large cache sizes. One blocking
/// hop per account does both the top-contacts read and the histogram scan,
/// and the replies are joined in a `glib::spawn_future_local`; the
/// calendar-state half is pure in-memory reads and happens at reply time.
/// A reply whose `dashboard_generation` a newer refresh has already bumped
/// past is a snapshot superseded before it landed and is discarded.
pub(crate) fn refresh_lookout_view(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    lookout_view: &Rc<crate::lookout_view::LookoutView>,
) {
    let receivers: Vec<async_channel::Receiver<DashboardCacheReply>> = state
        .borrow()
        .accounts
        .values()
        .filter_map(|h| h.address_cache.clone())
        .map(|cache| {
            spawn_cache_read(worker, cache, |cache| {
                let contacts = cache.top_addresses(crate::lookout_view::TOP_CONTACTS_LIMIT).unwrap_or_default();
                let histogram = cache.hour_histogram(None).unwrap_or_default();
                (contacts, histogram)
            })
        })
        .collect();
    let generation = {
        let mut cs = calendar_state.borrow_mut();
        cs.dashboard_generation += 1;
        cs.dashboard_generation
    };
    let calendar_state = calendar_state.clone();
    let lookout_view = lookout_view.clone();
    glib::spawn_future_local(async move {
        let mut contacts: Vec<(EmailAddress, i64)> = Vec::new();
        let mut histogram = [0i64; 24];
        for rx in receivers {
            if let Ok((account_contacts, account_histogram)) = rx.recv().await {
                contacts.extend(account_contacts);
                for (i, count) in account_histogram.iter().enumerate() {
                    histogram[i] += count;
                }
            }
        }
        // A newer refresh started while these reads were in flight - this
        // snapshot is superseded, don't let it land over the newer one.
        if calendar_state.borrow().dashboard_generation != generation {
            return;
        }
        contacts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        contacts.truncate(crate::lookout_view::TOP_CONTACTS_LIMIT);
        set_lookout_data(&calendar_state, &lookout_view, contacts, histogram);
    });
}

/// Builds the dashboard's `LookoutData` from the worker-fetched mail stats
/// plus the in-memory calendar state and repaints the view - the reply-time
/// half of `refresh_lookout_view`.
pub(crate) fn set_lookout_data(
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    lookout_view: &Rc<crate::lookout_view::LookoutView>,
    contacts: Vec<(EmailAddress, i64)>,
    histogram: [i64; 24],
) {
    let (tasks, events, checked_calendar_ids, colors) = {
        let st = calendar_state.borrow();
        (
            merged_tasks(calendar_state),
            // The dashboard unions *every* cached month (pruned to the
            // upcoming-events window by `insert_dashboard_occurrences`), so
            // its 14-day horizon stays populated as events pass instead of
            // draining to whatever single month the sessions' polls last
            // synced.
            st.accounts
                .values()
                .flat_map(|h| h.occurrences_by_month.values().flatten().cloned())
                .chain(st.webcal_handles.values().flat_map(|h| h.occurrences_by_month.values().flatten().cloned()))
                .chain(st.birthdays.as_ref().into_iter().flat_map(|h| h.occurrences_by_month.values().flatten().cloned()))
                .collect(),
            st.checked_calendar_ids.clone(),
            st.calendar_colors.clone(),
        )
    };

    crate::lookout_view::set_data(
        lookout_view,
        &crate::lookout_view::LookoutData {
            contacts,
            histogram,
            tasks,
            events,
            checked_calendar_ids,
            colors,
            flagged_mail: calendar_state.borrow().flagged_mail.clone(),
        },
    );
}

/// Asks every calendar source to fetch the current and next three months,
/// so the upcoming-events surfaces (the dashboard card and the mail
/// overview panel's 90-day section) can reach past the month each session
/// last fetched on its 5-minute cadence. Uses the fetch-only `FetchMonth`
/// commands - deliberately *not* `SyncMonth` - so the sessions' polled
/// month (whatever the calendar view is showing) is left alone; hijacking
/// it would starve the mail-overview day list and the calendar tab.
pub(crate) fn widen_calendar_sync_horizon(calendar_state: &Rc<RefCell<CalendarUiState>>) {
    let mut st = calendar_state.borrow_mut();
    if let Some(birthdays) = &mut st.birthdays {
        birthdays.sync_dashboard_window();
    }
    for handle in st.accounts.values() {
        for month in dashboard_month_window() {
            let _ = handle.cmd_tx.send_blocking(CalendarCommand::FetchMonth(month));
        }
    }
    if let Some(cmd_tx) = &st.webcal_cmd_tx {
        for month in dashboard_month_window() {
            let _ = cmd_tx.send_blocking(SubscriptionCommand::FetchMonth(month));
        }
    }
}

/// The months the upcoming-events surfaces care about - the current month
/// and the next three, which together cover the 90-day horizon of the
/// mail overview panel's "Upcoming events" section (and, in practice, the
/// dashboard's shorter one). Month-normalized firsts so map keys and
/// session sync commands line up.
pub(crate) fn dashboard_month_window() -> [chrono::NaiveDate; 4] {
    let current = first_of_month(crate::calendar_tz::display_zone().today());
    [
        current,
        current + chrono::Months::new(1),
        current + chrono::Months::new(2),
        current + chrono::Months::new(3),
    ]
}

/// Stashes one synced month's occurrences in an upcoming-events map,
/// pruning any month outside the upcoming window (current through three
/// months ahead) so a long-lived session never accumulates stale month
/// buckets.
pub(crate) fn insert_dashboard_occurrences(map: &mut HashMap<chrono::NaiveDate, Vec<EventOccurrence>>, month: chrono::NaiveDate, occurrences: Vec<EventOccurrence>) {
    map.insert(month, occurrences);
    let window = dashboard_month_window();
    map.retain(|m, _| *m >= window[0] && *m <= window[window.len() - 1]);
}

/// Routes a completion-toggle (list checkbox) to the task's store. A recurring
/// task rolls forward in place to its next due date (T-06) instead of staying
/// completed; the toast's Undo restores the pre-toggle task. Every other
/// toggle is the plain status/completed/percent update the row built.
pub(crate) fn route_task_toggle(
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    toast_overlay: &adw::ToastOverlay,
    previous: CalendarTask,
    mut task: CalendarTask,
    completed: bool,
) {
    let rolled = completed && crate::task_recurrence::roll_forward_if_completed(Some(&previous), &mut task);
    let next_due = task.due;
    route_task_save(calendar_state, tasks_view, task.calendar_id.clone(), task);
    if rolled {
        let message = next_due.map_or_else(
            || "Task completed".to_string(),
            |due| {
                format!(
                    "Task completed \u{b7} next due {}",
                    crate::calendar_tz::display_zone().occurrence_to_local(due, false).format("%a %d %b")
                )
            },
        );
        let toast = adw::Toast::new(&message);
        toast.set_button_label(Some("Undo"));
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        toast.connect_button_clicked(move |_| {
            // The completion write already rotated the resource's etag; drop
            // it on the restore so an undo isn't rejected as a stale update.
            let mut restore = previous.clone();
            restore.etag = None;
            route_task_save(&calendar_state, &tasks_view, restore.calendar_id.clone(), restore);
        });
        toast_overlay.add_toast(toast);
    }
}

/// Opens the task editor for a brand-new task. The picker lists every
/// task-capable source; when none exists it falls back to a single "Local
/// (this device)" entry, so New task always has somewhere to go.
pub(crate) fn show_new_task_editor(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
) {
    let mut calendars = pickable_task_calendars(calendar_state);
    if calendars.is_empty() {
        calendars.push(("Local · Tasks".to_string(), local_tasks_calendar_id()));
    }
    let default_calendar = default_pickable_task_calendar(calendar_state).unwrap_or_else(local_tasks_calendar_id);
    let categories = state.borrow().app_config.borrow().categories.clone();
    let calendar_state = calendar_state.clone();
    let tasks_view = tasks_view.clone();
    crate::task_editor::show_task_editor(
        window,
        crate::task_editor::TaskEditorPrefill {
            calendars: &calendars,
            default_calendar,
            existing: None,
            prefill: None,
            categories: &categories,
        },
        move |calendar_id, task| route_task_save(&calendar_state, &tasks_view, calendar_id, task),
        move |_calendar_id, _uid, _href, _etag| {},
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn show_calendar_dialog(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    calendar_main: &Rc<CalendarMain>,
    calendar_list_box: &gtk::Box,
    mini_calendar: &calendar_view::MiniCalendar,
    mail_overview_day: &Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: &gtk::Box,
    toast_overlay: &adw::ToastOverlay,
) {
    let app_config = state.borrow().app_config.clone();

    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title("Add calendar")
        .default_width(460)
        .default_height(560)
        .build();

    // --- Subscribe section: URL (+ optional name) -> validated, persisted,
    // handed to the session.
    let url_entry = adw::EntryRow::builder().title("Feed URL").build();
    let name_entry = adw::EntryRow::builder().title("Name").build();
    let subscribe_button = gtk::Button::with_label("Subscribe");
    subscribe_button.add_css_class("suggested-action");
    subscribe_button.set_halign(gtk::Align::End);
    let subscribe_group = adw::PreferencesGroup::builder().title("Subscribe to a calendar").build();
    subscribe_group.add(&url_entry);
    subscribe_group.add(&name_entry);
    subscribe_group.add(&subscribe_button);

    // --- Import section: chosen .ics file -> parsed -> routed into the
    // picked CalDAV calendar (upsert by UID).
    let import_file_button = gtk::Button::with_label("Choose .ics file…");
    import_file_button.set_halign(gtk::Align::Start);
    let import_file_label = gtk::Label::builder()
        .label("No file chosen")
        .css_classes(["dim-label", "caption"])
        .xalign(0.0)
        .wrap(true)
        .hexpand(true)
        .build();
    // It becomes the chosen file's name, so keep it content, not chrome.
    crate::privacy::mark_content(&import_file_label);
    let chosen_ics: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let pickable = pickable_calendars(calendar_state);
    let import_calendar_labels: Vec<String> = pickable.iter().map(|(label, _)| label.clone()).collect();
    let import_calendar_ids: Vec<CalendarId> = pickable.iter().map(|(_, id)| id.clone()).collect();
    let import_target_dropdown = gtk::DropDown::builder().build();
    // Calendar names are content: blurred by privacy mode.
    crate::privacy::mark_content(&import_target_dropdown);
    if import_calendar_ids.is_empty() {
        import_target_dropdown.set_sensitive(false);
    } else {
        let label_refs: Vec<&str> = import_calendar_labels.iter().map(String::as_str).collect();
        import_target_dropdown.set_model(Some(&gtk::StringList::new(&label_refs)));
        import_target_dropdown.set_selected(0);
    }
    import_target_dropdown.set_tooltip_text(Some("Import into this calendar"));
    let import_button = gtk::Button::with_label("Import");
    import_button.add_css_class("suggested-action");
    import_button.set_sensitive(false);
    import_button.set_halign(gtk::Align::End);
    let import_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    import_row.append(&import_file_button);
    import_row.append(&import_file_label);
    let import_row2 = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    import_row2.append(&import_target_dropdown);
    import_row2.append(&import_button);
    let import_group = adw::PreferencesGroup::builder().title("Import from file").build();
    import_group.add(&import_row);
    import_group.add(&import_row2);

    // --- Manage section: one row per subscription (name + Remove), and a
    // manual refresh for feeds that shouldn't wait for the 5-minute poll.
    let manage_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let refresh_now_button = gtk::Button::with_label("Refresh now");
    refresh_now_button.add_css_class("flat");
    refresh_now_button.set_halign(gtk::Align::Start);
    let manage_group = adw::PreferencesGroup::builder().title("Manage subscriptions").build();
    manage_group.add(&manage_box);
    manage_group.add(&refresh_now_button);

    // After any change: repaint the checklist/view/mini-calendar from the
    // current state (the session's own `SubscriptionsUpdated` also lands,
    // but this makes the change feel immediate - the removed handle is
    // already gone from state, so its events vanish right away).
    let refresh_ui: Rc<dyn Fn()> = Rc::new({
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let calendar_list_box = calendar_list_box.clone();
        let mini_calendar = mini_calendar.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        move || {
            refresh_calendar_checklist(&calendar_state, &calendar_list_box, &calendar_main);
            refresh_displayed_calendar_view(&calendar_state, &calendar_main);
            refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
            let month = calendar_state.borrow().displayed_month;
            let event_days = calendar_event_days(&calendar_state, month);
            calendar_view::set_mini_event_days(&mini_calendar, &event_days);
        }
    });

    // Pushes the authoritative subscription list (from `settings.json`) to
    // the webcal session, which adopts it and re-syncs every feed.
    let reload_session: Rc<dyn Fn()> = Rc::new({
        let calendar_state = calendar_state.clone();
        let app_config = app_config.clone();
        move || {
            let subscriptions = app_config.borrow().webcal_subscriptions.clone();
            let cmd = calendar_state.borrow().webcal_cmd_tx.clone();
            if let Some(cmd) = cmd {
                let _ = cmd.send_blocking(SubscriptionCommand::Reload { subscriptions });
            }
        }
    });

    // Re-renders the manage list from current state (called at open and
    // after every subscribe/remove). The two closures are mutually
    // referential - `rebuild_manage` creates rows whose Remove buttons call
    // `remove_subscription`, which re-renders via `rebuild_manage` - so the
    // rebuild is stashed in a slot the remove handler fills in afterwards.
    let rebuild_manage: ManageRebuild = Rc::new(RefCell::new(None));
    let remove_subscription: Rc<dyn Fn(String)> = Rc::new({
        let app_config = app_config.clone();
        let calendar_state = calendar_state.clone();
        let reload_session = reload_session.clone();
        let refresh_ui = refresh_ui.clone();
        let rebuild_manage = rebuild_manage.clone();
        let toast_overlay = toast_overlay.clone();
        move |id: String| {
            {
                let mut config = app_config.borrow_mut();
                config.webcal_subscriptions.retain(|s| s.id != id);
                crate::app_config::save(&config);
            }
            {
                let mut st = calendar_state.borrow_mut();
                st.webcal_subscriptions.retain(|s| s.id != id);
                st.webcal_handles.remove(&id);
                st.checked_calendar_ids.remove(&webcal_calendar_id(&id));
                st.calendar_colors.remove(&webcal_calendar_id(&id));
            }
            calendar_colors::save(&calendar_state.borrow().calendar_colors);
            let _ = lookout_dav::remove_subscription_cache(&id);
            reload_session();
            refresh_ui();
            let rebuild = rebuild_manage.borrow().clone();
            if let Some(rebuild) = rebuild {
                rebuild();
            }
            toast_overlay.add_toast(adw::Toast::new("Subscription removed"));
        }
    });
    *rebuild_manage.borrow_mut() = Some(Rc::new({
        let manage_box = manage_box.clone();
        let calendar_state = calendar_state.clone();
        let remove_subscription = remove_subscription.clone();
        move || {
            while let Some(child) = manage_box.first_child() {
                manage_box.remove(&child);
            }
            let subs = calendar_state.borrow().webcal_subscriptions.clone();
            if subs.is_empty() {
                let label = gtk::Label::builder()
                    .label("No subscriptions yet")
                    .css_classes(["dim-label", "caption"])
                    .xalign(0.0)
                    .build();
                crate::privacy::mark_ui(&label);
                manage_box.append(&label);
            }
            for sub in subs {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let text = gtk::Label::builder()
                    .label(&sub.display_name)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .hexpand(true)
                    .xalign(0.0)
                    .build();
                text.set_tooltip_text(Some(&sub.url));
                let remove = gtk::Button::from_icon_name(themed_icon_name(&["user-trash-symbolic", "edit-delete-symbolic"]));
                remove.add_css_class("flat");
                remove.set_tooltip_text(Some("Remove subscription"));
                let id = sub.id.clone();
                let remove_subscription = remove_subscription.clone();
                remove.connect_clicked(move |_| remove_subscription(id.clone()));
                row.append(&text);
                row.append(&remove);
                manage_box.append(&row);
            }
        }
    }));

    // Persists one new subscription (a manual feed URL or a bundled holiday
    // feed) and hands the authoritative list to the webcal session. Shared by
    // the Subscribe button and the Holidays picker so both paths stay in step.
    let add_subscription: Rc<dyn Fn(WebcalSubscription)> = Rc::new({
        let app_config = app_config.clone();
        let calendar_state = calendar_state.clone();
        let reload_session = reload_session.clone();
        let refresh_ui = refresh_ui.clone();
        let rebuild_manage = rebuild_manage.clone();
        move |subscription: WebcalSubscription| {
            {
                let mut config = app_config.borrow_mut();
                config.webcal_subscriptions.push(subscription.clone());
                crate::app_config::save(&config);
            }
            {
                let mut st = calendar_state.borrow_mut();
                st.webcal_subscriptions.push(subscription.clone());
                st.webcal_handles.insert(
                    subscription.id.clone(),
                    WebcalHandle {
                        calendar_id: webcal_calendar_id(&subscription.id),
                        display_name: subscription.display_name.clone(),
                        last_occurrences: Vec::new(),
                        last_synced_month: None,
                        occurrences_by_month: HashMap::new(),
                        error: None,
                    },
                );
            }
            reload_session();
            refresh_ui();
            let rebuild = rebuild_manage.borrow().clone();
            if let Some(rebuild) = rebuild {
                rebuild();
            }
        }
    });

    subscribe_button.connect_clicked({
        let url_entry = url_entry.clone();
        let name_entry = name_entry.clone();
        let add_subscription = add_subscription.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let url_text = url_entry.text().trim().to_string();
            let Ok(url) = lookout_dav::normalize_webcal_url(&url_text) else {
                toast_overlay.add_toast(adw::Toast::new("That doesn't look like a calendar feed URL."));
                return;
            };
            let name = {
                let typed = name_entry.text().trim().to_string();
                if typed.is_empty() {
                    url.host_str().unwrap_or(&url_text).to_string()
                } else {
                    typed
                }
            };
            let subscription = WebcalSubscription {
                id: uuid::Uuid::new_v4().to_string(),
                display_name: name,
                url: url_text,
            };
            add_subscription(subscription);
            url_entry.set_text("");
            name_entry.set_text("");
            toast_overlay.add_toast(adw::Toast::new("Subscribed - fetching events…"));
        }
    });

    // --- Holidays section (item 3.15 / CAL-28): a country picker that
    // subscribes to that country's bundled public-holiday feed, reusing the
    // webcal path above. The table lives in `lookout_core::holidays`.
    let holiday_dropdown = gtk::DropDown::builder().build();
    let holiday_countries = lookout_core::holiday_countries();
    let holiday_labels: Vec<&str> = holiday_countries.iter().map(|country| country.name.as_str()).collect();
    holiday_dropdown.set_model(Some(&gtk::StringList::new(&holiday_labels)));
    holiday_dropdown.set_selected(0);
    holiday_dropdown.set_tooltip_text(Some("Country"));
    holiday_dropdown.set_hexpand(true);
    let holiday_button = gtk::Button::with_label("Subscribe");
    holiday_button.add_css_class("suggested-action");
    holiday_button.set_halign(gtk::Align::End);
    holiday_button.set_sensitive(!holiday_countries.is_empty());
    let holiday_group = adw::PreferencesGroup::builder().title("Holidays").build();
    holiday_group.add(&holiday_dropdown);
    holiday_group.add(&holiday_button);
    holiday_button.connect_clicked({
        let holiday_dropdown = holiday_dropdown.clone();
        let add_subscription = add_subscription.clone();
        let app_config = app_config.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let countries = lookout_core::holiday_countries();
            let Some(country) = countries.get(holiday_dropdown.selected() as usize) else {
                return;
            };
            let subscription = country.to_subscription();
            if lookout_core::is_holiday_subscribed(&subscription.url, &app_config.borrow().webcal_subscriptions) {
                toast_overlay.add_toast(adw::Toast::new(&format!("{} is already subscribed", subscription.display_name)));
                return;
            }
            let name = subscription.display_name.clone();
            add_subscription(subscription);
            toast_overlay.add_toast(adw::Toast::new(&format!("Subscribed to {name} - fetching events…")));
        }
    });

    import_file_button.connect_clicked({
        let window = window.clone();
        let import_file_label = import_file_label.clone();
        let chosen_ics = chosen_ics.clone();
        let import_button = import_button.clone();
        move |_| {
            let window = window.clone();
            let import_file_label = import_file_label.clone();
            let chosen_ics = chosen_ics.clone();
            let import_button = import_button.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_suffix("ics");
                filter.set_name(Some("iCalendar files (*.ics)"));
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let dialog = gtk::FileDialog::builder().title("Choose a calendar file").filters(&filters).build();
                let Ok(file) = dialog.open_future(Some(&window)).await else { return };
                let Some(path) = file.path() else { return };
                let Ok(raw) = std::fs::read(&path) else { return };
                *chosen_ics.borrow_mut() = Some(String::from_utf8_lossy(&raw).into_owned());
                import_file_label.set_label(&path.display().to_string());
                import_button.set_sensitive(true);
            });
        }
    });

    import_button.connect_clicked({
        let dialog = dialog.clone();
        let chosen_ics = chosen_ics.clone();
        let import_target_dropdown = import_target_dropdown.clone();
        let import_calendar_ids = import_calendar_ids.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let Some(ics) = chosen_ics.borrow().clone() else { return };
            let Some(calendar_id) = import_calendar_ids.get(import_target_dropdown.selected() as usize).cloned() else {
                toast_overlay.add_toast(adw::Toast::new("Choose a calendar to import into."));
                return;
            };
            let events = lookout_dav::parse_vevents(&CalendarId("import".to_string()), &ics);
            if events.is_empty() {
                toast_overlay.add_toast(adw::Toast::new("No events found in this file."));
                return;
            }
            let mut created = 0;
            let mut updated = 0;
            for mut event in events {
                event.calendar_id = calendar_id.clone();
                match find_occurrence_in_calendar(&calendar_state, &calendar_id, &event.uid) {
                    Some(existing) => {
                        event.href = existing.href;
                        event.etag = existing.etag;
                        updated += 1;
                    }
                    None => {
                        event.href = None;
                        event.etag = None;
                        created += 1;
                    }
                }
                route_calendar_save(&calendar_state, calendar_id.clone(), event);
            }
            let summary = if updated > 0 {
                format!("Imported {created} event(s), updated {updated} existing")
            } else {
                format!("Imported {created} event(s)")
            };
            toast_overlay.add_toast(adw::Toast::new(&summary));
            dialog.close();
        }
    });

    refresh_now_button.connect_clicked({
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let cmd = calendar_state.borrow().webcal_cmd_tx.clone();
            if let Some(cmd) = cmd {
                let _ = cmd.send_blocking(SubscriptionCommand::Refresh);
                toast_overlay.add_toast(adw::Toast::new("Refreshing calendar feeds…"));
            }
        }
    });

    // --- Open another user's calendar (item 5.6 / CAL-25): resolve the
    // address through Graph, list that user's calendars, and add the chosen
    // one to this account (the calendars sync in through
    // `CalendarCommand::SetSharedCalendars`). Only offered once a Microsoft 365
    // account with Graph access is connected.
    let shared_address_entry = adw::EntryRow::builder().title("Name or email address").build();
    let shared_continue_button = gtk::Button::with_label("Continue");
    shared_continue_button.set_halign(gtk::Align::End);
    let shared_status = gtk::Label::builder()
        .label("Enter someone's address to see their calendars")
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .build();
    crate::privacy::mark_ui(&shared_status);
    let shared_calendar_dropdown = gtk::DropDown::builder().build();
    // Shared calendar names are content: blurred by privacy mode.
    crate::privacy::mark_content(&shared_calendar_dropdown);
    shared_calendar_dropdown.set_sensitive(false);
    let shared_add_button = gtk::Button::with_label("Add");
    shared_add_button.add_css_class("suggested-action");
    shared_add_button.set_halign(gtk::Align::End);
    shared_add_button.set_sensitive(false);
    let shared_group = adw::PreferencesGroup::builder().title("Open another user's calendar").build();
    shared_group.add(&shared_address_entry);
    shared_group.add(&shared_continue_button);
    shared_group.add(&shared_status);
    shared_group.add(&shared_calendar_dropdown);
    shared_group.add(&shared_add_button);
    // (display name, owner id, calendar id) for each listed calendar.
    let shared_candidates: Rc<RefCell<Vec<(String, String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let shared_address: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    shared_continue_button.connect_clicked({
        let calendar_state = calendar_state.clone();
        let shared_address_entry = shared_address_entry.clone();
        let shared_status = shared_status.clone();
        let shared_calendar_dropdown = shared_calendar_dropdown.clone();
        let shared_add_button = shared_add_button.clone();
        let shared_candidates = shared_candidates.clone();
        let shared_address = shared_address.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let address = shared_address_entry.text().trim().to_string();
            if address.is_empty() {
                return;
            }
            let Some(parent) = first_shareable_calendar(&calendar_state) else {
                toast_overlay.add_toast(adw::Toast::new("Opening another user's calendar needs a Microsoft 365 account with Outlook access"));
                return;
            };
            let account_id = parent.account_id.clone();
            let client = crate::graph_calendar_share::GraphCalendarShareClient::new(account_id.clone());
            let shared_status = shared_status.clone();
            let shared_calendar_dropdown = shared_calendar_dropdown.clone();
            let shared_add_button = shared_add_button.clone();
            let shared_candidates = shared_candidates.clone();
            let shared_address = shared_address.clone();
            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                shared_status.set_label("Looking up…");
                let user = match client.resolve_user(&address).await {
                    Ok(Some(user)) => user,
                    Ok(None) => {
                        shared_status.set_label("No such person in your organization");
                        return;
                    }
                    Err(e) => {
                        shared_status.set_label("Couldn't look them up");
                        toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't look them up: {e}"))));
                        return;
                    }
                };
                match client.list_calendars(&account_id, &user.id).await {
                    Ok(calendars) if calendars.is_empty() => {
                        shared_status.set_label("They have no calendars you can open");
                    }
                    Ok(calendars) => {
                        let labels: Vec<String> = calendars.iter().map(|c| c.display_name.clone()).collect();
                        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                        shared_calendar_dropdown.set_model(Some(&gtk::StringList::new(&label_refs)));
                        shared_calendar_dropdown.set_selected(0);
                        shared_calendar_dropdown.set_sensitive(true);
                        shared_add_button.set_sensitive(true);
                        *shared_candidates.borrow_mut() = calendars.iter().map(|c| (c.display_name.clone(), user.id.clone(), c.href.clone())).collect();
                        *shared_address.borrow_mut() = address.clone();
                        shared_status.set_label(&format!("{} calendar(s) found", calendars.len()));
                    }
                    Err(e) => {
                        shared_status.set_label("Couldn't list their calendars");
                        toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't list their calendars: {e}"))));
                    }
                }
            });
        }
    });

    shared_add_button.connect_clicked({
        let calendar_state = calendar_state.clone();
        let app_config = app_config.clone();
        let shared_calendar_dropdown = shared_calendar_dropdown.clone();
        let shared_candidates = shared_candidates.clone();
        let shared_address = shared_address.clone();
        let shared_status = shared_status.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let Some(parent) = first_shareable_calendar(&calendar_state) else {
                return;
            };
            let index = shared_calendar_dropdown.selected() as usize;
            let Some((name, owner_id, calendar_id)) = shared_candidates.borrow().get(index).cloned() else {
                return;
            };
            let address = shared_address.borrow().clone();
            let shared = crate::app_config::SharedCalendar {
                id: format!("{}{}", crate::app_config::SHARED_CALENDAR_ID_PREFIX, uuid::Uuid::new_v4()),
                parent_account: parent.account_id.clone(),
                address,
                owner_id,
                calendar_id,
                display_name: name.clone(),
            };
            {
                let mut config = app_config.borrow_mut();
                config.shared_calendars.push(shared);
                crate::app_config::save(&config);
            }
            // Push the account's full owner set to the running Graph session.
            let owners: Vec<String> = app_config
                .borrow()
                .shared_calendars
                .iter()
                .filter(|sc| sc.parent_account == parent.account_id)
                .map(|sc| sc.owner_id.clone())
                .collect();
            if let Some(handle) = calendar_state.borrow().accounts.get(&parent.account_id) {
                let _ = handle.cmd_tx.send_blocking(CalendarCommand::SetSharedCalendars { owners });
            }
            shared_status.set_label(&format!("Added {name} - syncing…"));
            toast_overlay.add_toast(adw::Toast::new(&format!("Added {name} - syncing…")));
        }
    });

    // --- Manage the shared/delegated calendars already added, each with a
    // Remove that drops it from `settings.json` and re-pushes the account's
    // owner set to the running session.
    let shared_manage_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let shared_manage_group = adw::PreferencesGroup::builder().title("Shared calendars").build();
    shared_manage_group.add(&shared_manage_box);
    for shared in app_config.borrow().shared_calendars.clone() {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let label = gtk::Label::builder()
            .label(format!("{} ({})", shared.display_name, shared.address))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.add_css_class("flat");
        remove.set_tooltip_text(Some("Remove shared calendar"));
        remove.connect_clicked({
            let app_config = app_config.clone();
            let calendar_state = calendar_state.clone();
            let toast_overlay = toast_overlay.clone();
            let row = row.clone();
            let parent_account = shared.parent_account.clone();
            let id = shared.id.clone();
            move |_| {
                {
                    let mut config = app_config.borrow_mut();
                    config.shared_calendars.retain(|s| s.id != id);
                    crate::app_config::save(&config);
                }
                let owners: Vec<String> = app_config
                    .borrow()
                    .shared_calendars
                    .iter()
                    .filter(|sc| sc.parent_account == parent_account)
                    .map(|sc| sc.owner_id.clone())
                    .collect();
                if let Some(handle) = calendar_state.borrow().accounts.get(&parent_account) {
                    let _ = handle.cmd_tx.send_blocking(CalendarCommand::SetSharedCalendars { owners });
                }
                row.set_visible(false);
                toast_overlay.add_toast(adw::Toast::new("Shared calendar removed"));
            }
        });
        row.append(&label);
        row.append(&remove);
        shared_manage_box.append(&row);
    }
    if shared_manage_box.first_child().is_none() {
        let empty = gtk::Label::builder()
            .label("No shared calendars yet")
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build();
        crate::privacy::mark_ui(&empty);
        shared_manage_box.append(&empty);
    }

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    content.append(&subscribe_group);
    content.append(&shared_group);
    content.append(&shared_manage_group);
    content.append(&holiday_group);
    content.append(&import_group);
    content.append(&manage_group);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&content)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    dialog.set_content(Some(&scroller));
    let rebuild = rebuild_manage.borrow().clone();
    if let Some(rebuild) = rebuild {
        rebuild();
    }
    dialog.present();
}
