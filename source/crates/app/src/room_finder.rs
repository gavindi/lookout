//! The event editor's Microsoft 365 room finder (OUTLOOK_PARITY.md item 15 /
//! CAL-19).
//!
//! Outlook's Location field doubles as a room finder: it lists the tenant's
//! bookable rooms and each candidate's free/busy for the meeting window, and a
//! picked room both fills Location and books the room as a resource attendee.
//! This module is the editor's half of that - a suffix button on the Location
//! row plus a searchable popover - driven by [`RoomFinder`], a callback bridge
//! the caller implements over `lookout_graph` (see
//! `window::calendar_glue::room_finder_context`). The widget code never talks
//! to Graph itself, matching the editor's data-in/widget-state-out contract.
//!
//! CalDAV and webcal calendars have no room-directory standard, so the caller
//! only shows the button for a Graph-backed calendar. `Place.Read.All` is
//! admin-sensitive, so consent is requested only from the popover's explicit
//! **Allow room suggestions** action, and a blocked tenant degrades to the
//! free-text Location rather than an error dialog (§10).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, NaiveDate, Utc};
use gtk::glib;
use lookout_core::{AttendeeAvailability, BusyStatus, CalendarId, EmailAddress};

use crate::event_editor::{boundary_to_utc_in, date_time_from_form, selected_zone};
use crate::recipient_entry::RecipientEntry;

/// One bookable room shown by the Location row's room finder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSuggestion {
    pub display_name: String,
    pub address: String,
    pub capacity: Option<u32>,
    pub building: Option<String>,
}

impl From<lookout_graph::Room> for RoomSuggestion {
    fn from(room: lookout_graph::Room) -> Self {
        RoomSuggestion {
            display_name: room.display_name,
            address: room.address,
            capacity: room.capacity,
            building: room.building,
        }
    }
}

/// Why a room lookup couldn't answer. `ConsentRequired` stays distinct from a
/// hard failure so the popover offers the one-click Allow action instead of an
/// error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomLookupError {
    ConsentRequired,
    Unavailable(String),
}

/// A room-list answer: the tenant's rooms, or why they couldn't be read.
pub type RoomsLoaded = Rc<dyn Fn(Result<Vec<RoomSuggestion>, RoomLookupError>)>;
/// The interactive `Place.Read.All` consent flow's answer.
pub type ConsentFinished = Rc<dyn Fn(Result<(), String>)>;
/// Room free/busy: the request generation plus one answer per address.
pub type RoomAvailabilityLoaded = Rc<dyn Fn(u64, Vec<AttendeeAvailability>)>;

/// The room-finder free/busy request: calendar, candidate rooms, the meeting
/// window, a request generation and the answer callback.
pub type RoomAvailabilityLookup = Rc<dyn Fn(CalendarId, Vec<EmailAddress>, DateTime<Utc>, DateTime<Utc>, u64, RoomAvailabilityLoaded)>;

/// The meeting span, in UTC, or `None` while the form's times are invalid.
type RoomWindow = Rc<dyn Fn() -> Option<(DateTime<Utc>, DateTime<Utc>)>>;
/// The fetch-availability hook, filled once the closure exists (the room
/// loader calls it after the first tenant fetch).
type AvailabilityRefresh = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// The editor's room-finder bridge: the caller owns the account lookup,
/// consent and directory cache.
#[derive(Clone)]
pub struct RoomFinder {
    /// Lists the selected calendar's tenant rooms.
    pub load: Rc<dyn Fn(CalendarId, RoomsLoaded)>,
    /// Runs the interactive `Place.Read.All` consent flow (Allow button only).
    pub grant: Rc<dyn Fn(CalendarId, ConsentFinished)>,
    /// Free/busy for candidate rooms over a meeting window.
    pub availability: RoomAvailabilityLookup,
}

/// Whether `room` matches a room-search query, over its name, address and
/// building (case-insensitively).
fn room_matches(room: &RoomSuggestion, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    room.display_name.to_lowercase().contains(&query)
        || room.address.to_lowercase().contains(&query)
        || room.building.as_deref().is_some_and(|building| building.to_lowercase().contains(&query))
}

/// A room row's subtitle: address, seats and building, in Outlook's order.
fn room_subtitle(room: &RoomSuggestion) -> String {
    let mut parts = vec![room.address.clone()];
    if let Some(capacity) = room.capacity {
        parts.push(format!("{capacity} seats"));
    }
    if let Some(building) = room.building.as_deref().filter(|building| !building.trim().is_empty()) {
        parts.push(building.to_string());
    }
    parts.join(" · ")
}

/// A candidate room's availability for the meeting span: Outlook's wording
/// plus the CSS class that colours it.
fn room_availability_status(availability: Option<&AttendeeAvailability>, start: DateTime<Utc>, end: DateTime<Utc>, checking: bool) -> (&'static str, &'static str) {
    let Some(availability) = availability else {
        // A lookup is in flight for the first time; once it has answered, a
        // room with no entry simply wasn't checked (the per-refresh cap).
        return (if checking { "Checking…" } else { "Unknown" }, "dim-label");
    };
    if availability.unknown {
        return ("Unknown", "dim-label");
    }
    match availability.conflict(start, end) {
        None => ("Free", "success"),
        Some(interval) => match interval.status {
            BusyStatus::Tentative => ("Tentative", "warning"),
            BusyStatus::Oof => ("Away", "error"),
            _ => ("Busy", "error"),
        },
    }
}

/// The widgets the editor needs after the room finder is attached: the button
/// (for read-only hiding) and the address set that makes a chosen room a
/// resource attendee at save time.
pub struct RoomFinderWidgets {
    pub button: gtk::Button,
    pub resource_attendees: Rc<RefCell<HashSet<String>>>,
}

/// The editor widgets and the selected-calendar state the finder reads.
pub struct RoomFinderInputs<'a> {
    pub location_row: &'a adw::EntryRow,
    pub attendees_field: &'a RecipientEntry,
    pub calendar_dropdown: &'a gtk::DropDown,
    pub calendar_ids: &'a [CalendarId],
    pub graph_calendars: &'a HashSet<CalendarId>,
    pub start_date: &'a Rc<Cell<NaiveDate>>,
    pub end_date: &'a Rc<Cell<NaiveDate>>,
    pub start_hour: &'a gtk::SpinButton,
    pub start_minute: &'a gtk::SpinButton,
    pub end_hour: &'a gtk::SpinButton,
    pub end_minute: &'a gtk::SpinButton,
    pub start_calendar: &'a gtk::Calendar,
    pub end_calendar: &'a gtk::Calendar,
    pub all_day_switch: &'a gtk::Switch,
    pub zone_dropdown: &'a gtk::DropDown,
    pub finder: RoomFinder,
}

/// Adds the **Find a room** suffix button and its popover to the Location row.
pub fn attach(inputs: RoomFinderInputs<'_>) -> RoomFinderWidgets {
    let RoomFinderInputs {
        location_row,
        attendees_field,
        calendar_dropdown,
        calendar_ids,
        graph_calendars,
        start_date,
        end_date,
        start_hour,
        start_minute,
        end_hour,
        end_minute,
        start_calendar,
        end_calendar,
        all_day_switch,
        zone_dropdown,
        finder,
    } = inputs;
    let location_row = location_row.clone();
    let attendees_field = attendees_field.clone();
    let calendar_dropdown = calendar_dropdown.clone();
    let calendar_ids = calendar_ids.to_vec();
    let graph_calendars = graph_calendars.clone();
    let start_date = start_date.clone();
    let end_date = end_date.clone();
    let start_hour = start_hour.clone();
    let start_minute = start_minute.clone();
    let end_hour = end_hour.clone();
    let end_minute = end_minute.clone();
    let start_calendar = start_calendar.clone();
    let end_calendar = end_calendar.clone();
    let all_day_switch = all_day_switch.clone();
    let zone_dropdown = zone_dropdown.clone();

    let resource_attendees: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
    let button = gtk::Button::builder()
        .icon_name("edit-find-symbolic")
        .tooltip_text("Find a room")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label("Find a room")]);
    location_row.add_suffix(&button);

    let popover = gtk::Popover::builder().autohide(true).has_arrow(true).build();
    popover.set_parent(&button);
    let heading = gtk::Label::builder().label("Rooms").xalign(0.0).css_classes(["heading"]).build();
    crate::privacy::mark_ui(&heading);
    let search = gtk::SearchEntry::builder().placeholder_text("Search rooms").build();
    let allow_button = gtk::Button::with_label("Allow room suggestions");
    allow_button.add_css_class("suggested-action");
    allow_button.set_halign(gtk::Align::Start);
    allow_button.set_visible(false);
    let status = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label", "caption"]).build();
    crate::privacy::mark_ui(&status);
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(320)
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .width_request(340)
        .build();
    content.append(&heading);
    content.append(&search);
    content.append(&allow_button);
    content.append(&status);
    content.append(&scroller);
    popover.set_child(Some(&content));

    // The editor owns the tenant's rooms for its lifetime: one fetch per
    // editor open, filtered locally thereafter. Availability follows the
    // meeting window and is dropped whenever the calendar changes.
    let rooms: Rc<RefCell<Vec<RoomSuggestion>>> = Rc::new(RefCell::new(Vec::new()));
    let loading = Rc::new(Cell::new(false));
    let consent_needed = Rc::new(Cell::new(false));
    let consent_attempted = Rc::new(Cell::new(false));
    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let availability: Rc<RefCell<HashMap<String, AttendeeAvailability>>> = Rc::new(RefCell::new(HashMap::new()));
    let checking = Rc::new(Cell::new(false));
    let generation = Rc::new(Cell::new(0u64));
    // Bumped when the calendar changes, so an in-flight tenant fetch can't
    // land another account's rooms in the popover.
    let room_generation = Rc::new(Cell::new(0u64));

    let selected_calendar: Rc<dyn Fn() -> Option<CalendarId>> = {
        let calendar_dropdown = calendar_dropdown.clone();
        let calendar_ids = calendar_ids.clone();
        Rc::new(move || calendar_ids.get(calendar_dropdown.selected() as usize).cloned())
    };

    // The meeting span, in UTC, for the rooms' free/busy. `None` while the
    // form's times are invalid (the Save path reports that separately).
    let current_window: RoomWindow = {
        let start_date = start_date.clone();
        let end_date = end_date.clone();
        let start_hour = start_hour.clone();
        let start_minute = start_minute.clone();
        let end_hour = end_hour.clone();
        let end_minute = end_minute.clone();
        let all_day_switch = all_day_switch.clone();
        let zone_dropdown = zone_dropdown.clone();
        Rc::new(move || {
            let zone = selected_zone(&zone_dropdown);
            let (start, end) = if all_day_switch.is_active() {
                let day = start_date.get().and_hms_opt(0, 0, 0)?;
                (boundary_to_utc_in(zone, day, false), boundary_to_utc_in(zone, day + chrono::Duration::days(1), false))
            } else {
                let start_local = date_time_from_form(&start_date, &start_hour, &start_minute, false);
                let end_local = date_time_from_form(&end_date, &end_hour, &end_minute, false);
                (boundary_to_utc_in(zone, start_local, false), boundary_to_utc_in(zone, end_local, false))
            };
            (end > start).then_some((start, end))
        })
    };

    // Picking a room fills Location and books the room as a resource
    // attendee, which is what makes Exchange/Graph reserve it (and what the
    // Scheduling tab then shows availability for).
    let pick: Rc<dyn Fn(&RoomSuggestion)> = {
        let location_row = location_row.clone();
        let attendees_field = attendees_field.clone();
        let resource_attendees = resource_attendees.clone();
        let popover = popover.clone();
        Rc::new(move |room: &RoomSuggestion| {
            location_row.set_text(&room.display_name);
            resource_attendees.borrow_mut().insert(room.address.to_ascii_lowercase());
            attendees_field.append_addresses(&[EmailAddress {
                name: Some(room.display_name.clone()),
                address: room.address.clone(),
            }]);
            popover.popdown();
        })
    };

    // Repaints the popover's list from the cached rooms, the query and the
    // latest availability answer.
    let rebuild: Rc<dyn Fn()> = {
        let search = search.clone();
        let status = status.clone();
        let allow_button = allow_button.clone();
        let list = list.clone();
        let rooms = rooms.clone();
        let loading = loading.clone();
        let consent_needed = consent_needed.clone();
        let failure = failure.clone();
        let availability = availability.clone();
        let checking = checking.clone();
        let current_window = current_window.clone();
        let pick = pick.clone();
        Rc::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            if consent_needed.get() {
                allow_button.set_visible(true);
                status.set_text("Room suggestions need permission to read your organization's rooms.");
                return;
            }
            allow_button.set_visible(false);
            if let Some(message) = failure.borrow().as_ref() {
                status.set_text(message);
                return;
            }
            if loading.get() {
                status.set_text("Loading rooms…");
                return;
            }
            if rooms.borrow().is_empty() {
                status.set_text("No rooms are available for this account.");
                return;
            }
            let query = search.text();
            let matching: Vec<RoomSuggestion> = rooms.borrow().iter().filter(|room| room_matches(room, &query)).take(50).cloned().collect();
            if matching.is_empty() {
                status.set_text("No rooms match your search.");
            } else {
                status.set_text("");
            }
            let window = current_window();
            let availability = availability.borrow();
            for room in matching {
                let title = glib::markup_escape_text(&room.display_name);
                let subtitle = glib::markup_escape_text(&room_subtitle(&room));
                let row = adw::ActionRow::builder().title(title.as_str()).subtitle(subtitle.as_str()).activatable(true).build();
                crate::privacy::mark_row(&row);
                let (label, class) = match window {
                    Some((start, end)) => room_availability_status(availability.get(&room.address.to_ascii_lowercase()), start, end, checking.get()),
                    None => ("Unknown", "dim-label"),
                };
                let availability_label = gtk::Label::builder().label(label).css_classes([class]).build();
                crate::privacy::mark_ui(&availability_label);
                row.add_suffix(&availability_label);
                let pick = pick.clone();
                let room = room.clone();
                row.connect_activated(move |_| pick(&room));
                list.append(&row);
            }
        })
    };

    // Set once availability can be asked for; a room load refreshes it, so the
    // first open can load and then fill the rows in one pass.
    let availability_refresh: AvailabilityRefresh = Rc::new(RefCell::new(None));

    let load_rooms: Rc<dyn Fn()> = {
        let finder = finder.clone();
        let rooms = rooms.clone();
        let loading = loading.clone();
        let consent_needed = consent_needed.clone();
        let consent_attempted = consent_attempted.clone();
        let failure = failure.clone();
        let rebuild = rebuild.clone();
        let selected_calendar = selected_calendar.clone();
        let availability_refresh = availability_refresh.clone();
        let room_generation = room_generation.clone();
        Rc::new(move || {
            if loading.get() {
                return;
            }
            let Some(calendar_id) = selected_calendar() else {
                return;
            };
            let requested = room_generation.get();
            loading.set(true);
            consent_needed.set(false);
            *failure.borrow_mut() = None;
            rebuild();
            let rooms = rooms.clone();
            let loading = loading.clone();
            let consent_needed = consent_needed.clone();
            let consent_attempted = consent_attempted.clone();
            let failure = failure.clone();
            let rebuild = rebuild.clone();
            let availability_refresh = availability_refresh.clone();
            let room_generation = room_generation.clone();
            (finder.load)(
                calendar_id,
                Rc::new(move |result| {
                    if requested != room_generation.get() {
                        return;
                    }
                    loading.set(false);
                    match result {
                        Ok(list) => *rooms.borrow_mut() = list,
                        // No interactive flow from a load: a query or open must
                        // never launch a browser. The Allow button runs it, and
                        // if it has already run then a still denied scope is
                        // the tenant's policy.
                        Err(RoomLookupError::ConsentRequired) if !consent_attempted.get() => consent_needed.set(true),
                        Err(RoomLookupError::ConsentRequired) => *failure.borrow_mut() = Some("Your organization doesn't allow room suggestions.".to_string()),
                        Err(RoomLookupError::Unavailable(message)) => *failure.borrow_mut() = Some(format!("Couldn't load rooms: {message}")),
                    }
                    rebuild();
                    let refresh = availability_refresh.borrow().clone();
                    if let Some(refresh) = refresh {
                        refresh();
                    }
                }),
            );
        })
    };

    let fetch_availability: Rc<dyn Fn()> = {
        let finder = finder.clone();
        let rooms = rooms.clone();
        let availability = availability.clone();
        let generation = generation.clone();
        let current_window = current_window.clone();
        let rebuild = rebuild.clone();
        let selected_calendar = selected_calendar.clone();
        let consent_needed = consent_needed.clone();
        let checking = checking.clone();
        Rc::new(move || {
            if consent_needed.get() || rooms.borrow().is_empty() {
                return;
            }
            let Some(calendar_id) = selected_calendar() else {
                return;
            };
            let Some((start, end)) = current_window() else {
                return;
            };
            let addresses: Vec<EmailAddress> = rooms
                .borrow()
                .iter()
                .take(crate::graph_rooms::MAX_ROOM_AVAILABILITY)
                .map(|room| EmailAddress::new(room.address.clone()))
                .collect();
            let next = generation.get() + 1;
            generation.set(next);
            checking.set(true);
            rebuild();
            let availability = availability.clone();
            let generation = generation.clone();
            let rebuild = rebuild.clone();
            let checking = checking.clone();
            (finder.availability)(
                calendar_id,
                addresses,
                start,
                end,
                next,
                Rc::new(move |answered, answers| {
                    if answered != generation.get() {
                        return;
                    }
                    checking.set(false);
                    let mut map = availability.borrow_mut();
                    map.clear();
                    map.extend(answers.into_iter().map(|answer| (answer.address.address.to_ascii_lowercase(), answer)));
                    drop(map);
                    rebuild();
                }),
            );
        })
    };
    *availability_refresh.borrow_mut() = Some(fetch_availability.clone());

    // The button opens the popover and kicks off the one fetch; the Allow
    // button runs the interactive `Place.Read.All` consent flow.
    {
        let popover = popover.clone();
        let load_rooms = load_rooms.clone();
        let fetch_availability = fetch_availability.clone();
        let rooms = rooms.clone();
        button.connect_clicked(move |_| {
            popover.popup();
            if rooms.borrow().is_empty() {
                load_rooms();
            } else {
                fetch_availability();
            }
        });
    }
    {
        let finder = finder.clone();
        let selected_calendar = selected_calendar.clone();
        let consent_attempted = consent_attempted.clone();
        let consent_needed = consent_needed.clone();
        let failure = failure.clone();
        let load_rooms = load_rooms.clone();
        let rebuild = rebuild.clone();
        allow_button.connect_clicked(move |_| {
            let Some(calendar_id) = selected_calendar() else {
                return;
            };
            let load_rooms = load_rooms.clone();
            let consent_attempted = consent_attempted.clone();
            let consent_needed = consent_needed.clone();
            let failure = failure.clone();
            let rebuild = rebuild.clone();
            (finder.grant)(
                calendar_id,
                Rc::new(move |result| match result {
                    Ok(()) => {
                        consent_attempted.set(true);
                        load_rooms();
                    }
                    Err(message) => {
                        // Surface the failure rather than leaving the Allow
                        // prompt up; closing and reopening the popover asks for
                        // consent again.
                        consent_needed.set(false);
                        *failure.borrow_mut() = Some(format!("Couldn't enable room suggestions: {message}"));
                        rebuild();
                    }
                }),
            );
        });
    }
    {
        let rebuild = rebuild.clone();
        search.connect_search_changed(move |_| rebuild());
    }

    // A changed span refreshes the rows' free/busy while the popover is open;
    // a closed popover pays nothing.
    let refresh_if_open: Rc<dyn Fn()> = {
        let popover = popover.clone();
        let fetch_availability = fetch_availability.clone();
        Rc::new(move || {
            if popover.is_visible() {
                fetch_availability();
            }
        })
    };
    for spin in [&start_hour, &start_minute, &end_hour, &end_minute] {
        let refresh = refresh_if_open.clone();
        spin.connect_value_changed(move |_| refresh());
    }
    {
        let refresh = refresh_if_open.clone();
        start_calendar.connect_day_selected(move |_| refresh());
    }
    {
        let refresh = refresh_if_open.clone();
        end_calendar.connect_day_selected(move |_| refresh());
    }
    {
        let refresh = refresh_if_open.clone();
        all_day_switch.connect_active_notify(move |_| refresh());
    }
    {
        let refresh = refresh_if_open.clone();
        zone_dropdown.connect_selected_notify(move |_| refresh());
    }

    // The room directory belongs to the selected calendar's account: a switch
    // to (or away from) a Graph calendar clears the cached tenant.
    let sync_room_button: Rc<dyn Fn()> = {
        let button = button.clone();
        let popover = popover.clone();
        let rooms = rooms.clone();
        let availability = availability.clone();
        let failure = failure.clone();
        let consent_needed = consent_needed.clone();
        let consent_attempted = consent_attempted.clone();
        let loading = loading.clone();
        let generation = generation.clone();
        let checking = checking.clone();
        let room_generation = room_generation.clone();
        let selected_calendar = selected_calendar.clone();
        let load_rooms = load_rooms.clone();
        let last_calendar: Rc<RefCell<Option<CalendarId>>> = Rc::new(RefCell::new(None));
        Rc::new(move || {
            let current = selected_calendar();
            let graph = current.as_ref().is_some_and(|id| graph_calendars.contains(id));
            button.set_visible(graph);
            if !graph {
                popover.popdown();
            }
            if *last_calendar.borrow() != current {
                *last_calendar.borrow_mut() = current;
                rooms.borrow_mut().clear();
                availability.borrow_mut().clear();
                *failure.borrow_mut() = None;
                consent_needed.set(false);
                consent_attempted.set(false);
                loading.set(false);
                checking.set(false);
                generation.set(generation.get() + 1);
                room_generation.set(room_generation.get() + 1);
                if graph && popover.is_visible() {
                    load_rooms();
                }
            }
        })
    };
    sync_room_button();
    {
        let sync = sync_room_button.clone();
        calendar_dropdown.connect_selected_notify(move |_| sync());
    }

    RoomFinderWidgets { button, resource_attendees }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room() -> RoomSuggestion {
        RoomSuggestion {
            display_name: "Boardroom".to_string(),
            address: "boardroom@example.com".to_string(),
            capacity: Some(12),
            building: Some("HQ".to_string()),
        }
    }

    #[test]
    fn room_search_matches_name_address_and_building_case_insensitively() {
        let room = room();
        assert!(room_matches(&room, ""));
        assert!(room_matches(&room, "board"));
        assert!(room_matches(&room, "BOARDROOM@EXAMPLE"));
        assert!(room_matches(&room, "hq"));
        assert!(!room_matches(&room, "lab"));
    }

    #[test]
    fn a_room_row_lists_address_seats_and_building() {
        assert_eq!(room_subtitle(&room()), "boardroom@example.com · 12 seats · HQ");
        let bare = RoomSuggestion {
            display_name: "Small".to_string(),
            address: "small@example.com".to_string(),
            capacity: None,
            building: None,
        };
        assert_eq!(room_subtitle(&bare), "small@example.com");
    }

    #[test]
    fn room_availability_reads_free_busy_against_the_meeting_span() {
        let start = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap().and_hms_opt(9, 0, 0).unwrap().and_utc();
        let end = start + chrono::Duration::hours(1);
        let room = EmailAddress::new("room@example.com");
        assert_eq!(room_availability_status(None, start, end, true).0, "Checking…");
        assert_eq!(room_availability_status(None, start, end, false).0, "Unknown");
        let unknown = AttendeeAvailability::unknown(room.clone());
        assert_eq!(room_availability_status(Some(&unknown), start, end, false).0, "Unknown");
        let free = AttendeeAvailability::new(room.clone(), Vec::new());
        assert_eq!(room_availability_status(Some(&free), start, end, false).0, "Free");
        let busy = AttendeeAvailability::new(
            room.clone(),
            vec![lookout_core::FreeBusyInterval {
                start: start + chrono::Duration::minutes(30),
                end: end + chrono::Duration::minutes(30),
                status: BusyStatus::Busy,
            }],
        );
        assert_eq!(room_availability_status(Some(&busy), start, end, false).0, "Busy");
        let tentative = AttendeeAvailability::new(
            room,
            vec![lookout_core::FreeBusyInterval {
                start,
                end,
                status: BusyStatus::Tentative,
            }],
        );
        assert_eq!(room_availability_status(Some(&tentative), start, end, false).0, "Tentative");
    }
}
