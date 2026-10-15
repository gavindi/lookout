//! The Automatic replies (Out-of-Office) settings dialog (Config → Mail →
//! Automatic replies, item 2.11 / M-24).
//!
//! One form edits one account at a time: turn replies on, optionally send
//! them only inside a time period, and write the message. Microsoft 365
//! accounts get the full Outlook shape - a message for people inside the
//! organization and one for outside, with an audience choice - because Graph
//! carries all of it; for an account on a ManageSieve server the script is
//! RFC 5230 `vacation`, which sends a single message, so the form shows one
//! message field and the out-of-organization controls are hidden. Servers that
//! can't host the feature (or the schedule) are detected through the session's
//! capability probe (`AccountCommand::FetchSieveCapabilities`) and the form
//! degrades instead of failing at save time.
//!
//! Saving writes straight to the account's backend: a Graph
//! `automaticRepliesSetting` PATCH for Microsoft 365, or `settings.json`
//! (`AppConfig::automatic_replies`) plus a Sieve script re-push for everything
//! else. The Mail banner (`window::refresh_automatic_replies_ui`) repaints
//! from whichever source applies.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, NaiveDate, NaiveTime, Timelike, Utc};
use gtk::glib;
use lookout_core::{AccountId, AutomaticReplies, AutomaticReplyStatus, ExternalAudience};

use crate::window::UiState;

/// Where one connected account keeps its automatic replies.
#[derive(Clone)]
enum RepliesBackend {
    /// Microsoft 365, through Graph (the client may not be consented yet).
    Graph(Rc<crate::graph_automatic_replies::GraphAutomaticRepliesClient>),
    /// A ManageSieve server: `settings.json` is the source of truth and the
    /// `vacation` block is compiled into the account's script.
    Sieve,
}

/// The connected accounts the dialog can configure: Microsoft 365 ones (Graph
/// present) and ManageSieve-advertising ones, labeled for the dropdown.
fn supported_accounts(state: &UiState) -> Vec<(AccountId, String, RepliesBackend)> {
    let mut accounts: Vec<(AccountId, String, RepliesBackend)> = state
        .accounts
        .iter()
        .filter_map(|(account_id, handle)| {
            let backend = match &handle.graph_automatic_replies {
                Some(client) => RepliesBackend::Graph(client.clone()),
                None if handle.sieve_advertised => RepliesBackend::Sieve,
                None => return None,
            };
            let label = if !handle.display_name.trim().is_empty() {
                handle.display_name.clone()
            } else if !handle.email.trim().is_empty() {
                handle.email.clone()
            } else {
                account_id.0.clone()
            };
            Some((account_id.clone(), label, backend))
        })
        .collect();
    accounts.sort_by_key(|(_, label, _)| label.to_lowercase());
    accounts
}

/// The settings a form starts from for one account.
fn load_settings(state: &UiState, account_id: &AccountId, backend: &RepliesBackend) -> AutomaticReplies {
    match backend {
        RepliesBackend::Graph(_) => state.automatic_replies.get(account_id).cloned().unwrap_or_default(),
        RepliesBackend::Sieve => state.app_config.borrow().automatic_replies_for(account_id).unwrap_or_default(),
    }
}

/// Presents the modal Automatic replies dialog for `state`'s connected
/// accounts.
pub(crate) fn show_automatic_replies_dialog(anchor: &gtk::Widget, state: Rc<RefCell<UiState>>) {
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Automatic replies").default_width(580).default_height(680);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let accounts = supported_accounts(&state.borrow());
    if accounts.is_empty() {
        let note = gtk::Label::builder()
            .label("Automatic replies are available for Microsoft 365 accounts that have enabled Outlook sync, and for mail servers that offer ManageSieve.")
            .wrap(true)
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build();
        root.append(&note);
        let done = gtk::Button::builder().label("Done").css_classes(["suggested-action"]).halign(gtk::Align::End).build();
        {
            let dialog = dialog.clone();
            done.connect_clicked(move |_| dialog.close());
        }
        root.append(&done);
        dialog.set_child(Some(&root));
        dialog.present();
        return;
    }

    let hint = gtk::Label::builder()
        .label("Replies are sent by your mail service, so they keep going out while Lookout is closed.")
        .wrap(true)
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();
    root.append(&hint);

    // --- Account picker. ---
    let labels: Vec<&str> = accounts.iter().map(|(_, label, _)| label.as_str()).collect();
    let account_dropdown = gtk::DropDown::from_strings(&labels);
    root.append(&labeled_row("Account", &account_dropdown));

    let backend_label = gtk::Label::builder().halign(gtk::Align::Start).css_classes(["dim-label", "caption"]).build();
    root.append(&backend_label);

    // --- The editable form, in one container so a not-yet-consented account
    // or an unsupported server can disable all of it at once. ---
    let form = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();

    let status_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    let period_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    let start_field = Rc::new(TimeField::new());
    let end_field = Rc::new(TimeField::new());
    let internal_view = message_view();
    let external_view = message_view();
    let audience_dropdown = gtk::DropDown::from_strings(&[ExternalAudience::All.label(), ExternalAudience::ContactsOnly.label(), ExternalAudience::None.label()]);

    let schedule_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    schedule_box.append(&start_field.widget("Start"));
    schedule_box.append(&end_field.widget("End"));

    let replies_section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    replies_section.append(&switch_row("Turn on automatic replies", &status_switch));
    replies_section.append(&switch_row("Send replies only during a time period", &period_switch));
    replies_section.append(&schedule_box);
    form.append(&section("Automatic replies", &replies_section));

    let message_caption = gtk::Label::builder().halign(gtk::Align::Start).css_classes(["dim-label", "caption"]).build();
    let message_section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    message_section.append(&message_caption);
    message_section.append(&gtk::ScrolledWindow::builder().height_request(110).child(&internal_view).build());
    form.append(&section("Message", &message_section));

    let external_section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    external_section.append(&labeled_row("Send to", &audience_dropdown));
    let external_caption = gtk::Label::builder()
        .label("Message for people outside your organization")
        .halign(gtk::Align::Start)
        .css_classes(["dim-label", "caption"])
        .build();
    external_section.append(&external_caption);
    external_section.append(&gtk::ScrolledWindow::builder().height_request(110).child(&external_view).build());
    form.append(&section("Outside your organization", &external_section));

    root.append(&form);

    let support_note = gtk::Label::builder().wrap(true).halign(gtk::Align::Start).css_classes(["dim-label", "caption"]).build();
    root.append(&support_note);

    // The Graph-not-consented state: a note and the consent button (the same
    // interactive flow Config's "Sync with Outlook" runs).
    let enable_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).halign(gtk::Align::Start).build();
    let enable_note = gtk::Label::builder()
        .label("Automatic replies for Microsoft 365 need Outlook sync, which this account hasn't enabled yet.")
        .wrap(true)
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();
    let enable_button = gtk::Button::with_label("Enable Outlook sync");
    enable_box.append(&enable_note);
    enable_box.append(&enable_button);
    root.append(&enable_box);

    let error_label = gtk::Label::builder().wrap(true).halign(gtk::Align::Start).css_classes(["error"]).visible(false).build();
    root.append(&error_label);

    let buttons = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).halign(gtk::Align::End).build();
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
    buttons.append(&cancel);
    buttons.append(&save);
    root.append(&buttons);

    // --- State shared by the closures. ---
    let current = Rc::new(Cell::new(0usize));
    let dirty = Rc::new(Cell::new(false));
    // Whether the current account's backend can host scheduled replies; the
    // reload pass sets it from the capability probe.
    let period_allowed = Rc::new(Cell::new(true));
    let probed: Rc<RefCell<HashSet<AccountId>>> = Rc::new(RefCell::new(HashSet::new()));
    let reload: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));

    // Keeps the schedule and out-of-organization chrome in step with the
    // switches and the audience choice.
    let update_visibility: Rc<dyn Fn()> = {
        let status_switch = status_switch.clone();
        let period_switch = period_switch.clone();
        let period_allowed = period_allowed.clone();
        let schedule_box = schedule_box.clone();
        let external_section = external_section.clone();
        let external_caption = external_caption.clone();
        let external_view = external_view.clone();
        let audience_dropdown = audience_dropdown.clone();
        let accounts = accounts.clone();
        let current = current.clone();
        Rc::new(move || {
            let on = status_switch.is_active();
            schedule_box.set_visible(on && period_switch.is_active());
            period_switch.set_sensitive(on && period_allowed.get());
            let graph = matches!(accounts.get(current.get()), Some((_, _, RepliesBackend::Graph(_))));
            external_section.set_visible(graph);
            let outside = audience_from_index(audience_dropdown.selected()) != ExternalAudience::None;
            external_caption.set_visible(outside);
            external_view.set_visible(outside);
        })
    };

    {
        let reload_handle = reload.clone();
        let state = state.clone();
        let accounts = accounts.clone();
        let current = current.clone();
        let dirty = dirty.clone();
        let period_allowed = period_allowed.clone();
        let probed = probed.clone();
        let backend_label = backend_label.clone();
        let form = form.clone();
        let status_switch = status_switch.clone();
        let period_switch = period_switch.clone();
        let start_field = start_field.clone();
        let end_field = end_field.clone();
        let internal_view = internal_view.clone();
        let external_view = external_view.clone();
        let audience_dropdown = audience_dropdown.clone();
        let support_note = support_note.clone();
        let enable_box = enable_box.clone();
        let message_caption = message_caption.clone();
        let save = save.clone();
        let update_visibility = update_visibility.clone();
        *reload.borrow_mut() = Box::new(move || {
            let Some((account_id, _, backend)) = accounts.get(current.get()) else {
                return;
            };
            let settings = load_settings(&state.borrow(), account_id, backend);

            // Backend-specific chrome and capability notes. `editable` gates
            // the whole form; `schedule_supported` gates the period switch.
            let mut editable = true;
            let mut schedule_supported = true;
            match backend {
                RepliesBackend::Graph(client) => {
                    backend_label.set_label("Microsoft 365");
                    set_label_if_needed(&message_caption, "Sent to people inside your organization");
                    let consented = client.consented();
                    enable_box.set_visible(!consented);
                    form.set_visible(consented);
                    support_note.set_visible(false);
                    editable = consented;
                }
                RepliesBackend::Sieve => {
                    backend_label.set_label("Server (Sieve)");
                    set_label_if_needed(&message_caption, "Sent to everyone while replies are on");
                    enable_box.set_visible(false);
                    form.set_visible(true);
                    match state.borrow().sieve_support.get(account_id).copied() {
                        Some(support) if !support.vacation => {
                            support_note.set_label("This server's Sieve service doesn't support automatic replies.");
                            support_note.set_visible(true);
                            editable = false;
                        }
                        Some(support) if !support.supports_scheduling() => {
                            support_note.set_label("This server doesn't support scheduled replies; replies are sent whenever they are on.");
                            support_note.set_visible(true);
                            schedule_supported = false;
                        }
                        Some(support) if !support.reachable => {
                            support_note.set_label("Couldn't reach the server's Sieve service to check what it supports; scheduling may not work.");
                            support_note.set_visible(true);
                        }
                        Some(_) => support_note.set_visible(false),
                        None => {
                            support_note.set_label("Checking the server's Sieve support…");
                            support_note.set_visible(true);
                            if probed.borrow_mut().insert(account_id.clone()) {
                                if let Some(handle) = state.borrow().accounts.get(account_id) {
                                    let _ = handle.cmd_tx.send_blocking(lookout_mail::session::AccountCommand::FetchSieveCapabilities);
                                }
                            }
                        }
                    }
                }
            }
            form.set_sensitive(editable);
            save.set_sensitive(editable);
            period_allowed.set(editable && schedule_supported);
            if !schedule_supported && period_switch.is_active() {
                period_switch.set_active(false);
            }

            // Values.
            status_switch.set_active(settings.enabled());
            period_switch.set_active(settings.status == AutomaticReplyStatus::Scheduled);
            let fallback = default_start();
            start_field.set(settings.start.unwrap_or(fallback));
            end_field.set(settings.end.unwrap_or(fallback + chrono::Duration::days(7)));
            set_message_text(&internal_view, &settings.internal_message);
            set_message_text(
                &external_view,
                if settings.external_message.is_empty() {
                    &settings.internal_message
                } else {
                    &settings.external_message
                },
            );
            audience_dropdown.set_selected(audience_index(settings.external_audience));
            update_visibility();
            dirty.set(false);

            // Microsoft 365: fetch the mailbox's current state when it isn't
            // cached yet, and repaint when it arrives (unless the user has
            // started editing).
            if let RepliesBackend::Graph(client) = backend {
                if client.consented() && !state.borrow().automatic_replies.contains_key(account_id) {
                    let state = state.clone();
                    let account_id = account_id.clone();
                    let client = client.clone();
                    let dirty = dirty.clone();
                    let reload = reload_handle.clone();
                    let current = current.clone();
                    let accounts = accounts.clone();
                    glib::spawn_future_local(async move {
                        if let Ok(fetched) = client.get().await {
                            state.borrow_mut().automatic_replies.insert(account_id.clone(), fetched);
                            let still_showing = accounts.get(current.get()).is_some_and(|(id, _, _)| id == &account_id);
                            if still_showing && !dirty.get() {
                                (reload.borrow())();
                            }
                        }
                    });
                }
            }
        });
    }
    (reload.borrow())();

    // Every edit marks the form dirty so a late fetch or capability answer
    // never clobbers typed-in text.
    for switch in [&status_switch, &period_switch] {
        let dirty = dirty.clone();
        let update_visibility = update_visibility.clone();
        switch.connect_active_notify(move |_| {
            dirty.set(true);
            update_visibility();
        });
    }
    for spin in [&start_field.hour, &start_field.minute, &end_field.hour, &end_field.minute] {
        let dirty = dirty.clone();
        spin.connect_value_changed(move |_| dirty.set(true));
    }
    for calendar in [&start_field.calendar, &end_field.calendar] {
        let dirty = dirty.clone();
        calendar.connect_day_selected(move |_| dirty.set(true));
    }
    for view in [&internal_view, &external_view] {
        let dirty = dirty.clone();
        view.buffer().connect_changed(move |_| dirty.set(true));
    }
    {
        let dirty = dirty.clone();
        let update_visibility = update_visibility.clone();
        audience_dropdown.connect_selected_notify(move |_| {
            dirty.set(true);
            update_visibility();
        });
    }
    {
        let current = current.clone();
        let reload = reload.clone();
        account_dropdown.connect_selected_notify(move |dropdown| {
            current.set(dropdown.selected() as usize);
            (reload.borrow())();
        });
    }
    {
        let state = state.clone();
        let accounts = accounts.clone();
        let current = current.clone();
        enable_button.connect_clicked(move |_| {
            if let Some((account_id, _, _)) = accounts.get(current.get()) {
                crate::window::enable_graph_automatic_replies(&state, account_id);
            }
        });
    }

    // The dialog registers itself so a capability answer or Graph fetch can
    // repaint it; it clears the hook when closed.
    {
        let reload = reload.clone();
        let dirty = dirty.clone();
        let hook: Rc<dyn Fn()> = Rc::new(move || {
            if !dirty.get() {
                (reload.borrow())();
            }
        });
        state.borrow_mut().automatic_replies_dialog_refresh = Some(hook);
    }
    dialog.connect_close_request({
        let state = state.clone();
        move |_| {
            state.borrow_mut().automatic_replies_dialog_refresh = None;
            glib::Propagation::Proceed
        }
    });

    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    {
        let state = state.clone();
        let accounts = accounts.clone();
        let current = current.clone();
        let status_switch = status_switch.clone();
        let period_switch = period_switch.clone();
        let start_field = start_field.clone();
        let end_field = end_field.clone();
        let internal_view = internal_view.clone();
        let external_view = external_view.clone();
        let audience_dropdown = audience_dropdown.clone();
        let error_label = error_label.clone();
        let dialog = dialog.clone();
        save.connect_clicked(move |_| {
            error_label.set_visible(false);
            let Some((account_id, _, backend)) = accounts.get(current.get()) else {
                return;
            };
            let on = status_switch.is_active();
            let period = on && period_switch.is_active();
            let status = if !on {
                AutomaticReplyStatus::Disabled
            } else if period {
                AutomaticReplyStatus::Scheduled
            } else {
                AutomaticReplyStatus::AlwaysEnabled
            };
            let internal_message = message_text(&internal_view);
            let (external_message, external_audience) = match backend {
                RepliesBackend::Graph(_) => (message_text(&external_view), audience_from_index(audience_dropdown.selected())),
                RepliesBackend::Sieve => (internal_message.clone(), ExternalAudience::All),
            };
            let settings = AutomaticReplies {
                status,
                start: start_field.value(),
                end: end_field.value(),
                internal_message,
                external_message,
                external_audience,
            };
            if let Err(error) = settings.validate() {
                error_label.set_label(error.message());
                error_label.set_visible(true);
                return;
            }
            match backend {
                RepliesBackend::Graph(client) => {
                    let state = state.clone();
                    let client = client.clone();
                    let account_id = account_id.clone();
                    let error_label = error_label.clone();
                    let dialog = dialog.clone();
                    glib::spawn_future_local(async move {
                        match client.set(&settings).await {
                            Ok(()) => {
                                state.borrow_mut().automatic_replies.insert(account_id.clone(), settings);
                                crate::window::refresh_automatic_replies_ui(&state);
                                crate::window::toast(&state, "Automatic replies updated");
                                dialog.close();
                            }
                            Err(e) => {
                                error_label.set_label(&format!("Couldn't save automatic replies: {e}"));
                                error_label.set_visible(true);
                            }
                        }
                    });
                }
                RepliesBackend::Sieve => {
                    let app_config = state.borrow().app_config.clone();
                    app_config.borrow_mut().upsert_automatic_replies(account_id.clone(), settings);
                    crate::app_config::save(&app_config.borrow());
                    crate::window::push_sieve_script(&state);
                    crate::window::refresh_automatic_replies_ui(&state);
                    crate::window::toast(&state, "Automatic replies updated");
                    dialog.close();
                }
            }
        });
    }

    dialog.set_child(Some(&gtk::ScrolledWindow::builder().vexpand(true).child(&root).build()));
    dialog.present();
}

/// A date-and-time field in the calendar's display zone: the shared date
/// button plus `hh:mm` spins (unlike the schedule-send picker, a past time is
/// as valid as a future one - replies may be set up retroactively).
struct TimeField {
    calendar: gtk::Calendar,
    date: Rc<Cell<NaiveDate>>,
    hour: gtk::SpinButton,
    minute: gtk::SpinButton,
}

impl TimeField {
    fn new() -> Self {
        let now = crate::calendar_tz::display_zone().now();
        let date = Rc::new(Cell::new(now.date()));
        let calendar = gtk::Calendar::new();
        crate::datetime_picker::set_calendar_date(&calendar, &date, now.date());
        crate::datetime_picker::wire_calendar(&calendar, &date);
        let hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
        let minute = gtk::SpinButton::with_range(0.0, 59.0, 1.0);
        for spin in [&hour, &minute] {
            spin.set_digits(0);
            spin.set_width_chars(2);
        }
        hour.set_value(f64::from(now.hour()));
        minute.set_value(f64::from(now.minute()));
        TimeField { calendar, date, hour, minute }
    }

    fn widget(&self, label: &str) -> gtk::Box {
        crate::datetime_picker::time_cluster(label, &self.calendar, &self.hour, &self.minute)
    }

    /// The field's wall time resolved to an instant in the display zone.
    fn value(&self) -> Option<DateTime<Utc>> {
        let time = NaiveTime::from_hms_opt(self.hour.value() as u32, self.minute.value() as u32, 0)?;
        Some(crate::calendar_tz::display_zone().resolve_local(self.date.get().and_time(time)))
    }

    fn set(&self, at: DateTime<Utc>) {
        let local = crate::calendar_tz::display_zone().to_local(at);
        crate::datetime_picker::set_calendar_date(&self.calendar, &self.date, local.date());
        self.hour.set_value(f64::from(local.hour()));
        self.minute.set_value(f64::from(local.minute()));
    }
}

/// The default schedule start: the next whole hour.
fn default_start() -> DateTime<Utc> {
    let now = crate::calendar_tz::display_zone().now();
    let base = now.date().and_hms_opt(now.hour(), 0, 0).unwrap_or(now);
    crate::calendar_tz::display_zone().resolve_local(base + chrono::Duration::hours(1))
}

/// A multi-line plain-text editor for one reply message.
fn message_view() -> gtk::TextView {
    gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .build()
}

/// The audience dropdown's index for `audience` (the order
/// [`ExternalAudience::label`] lists).
fn audience_index(audience: ExternalAudience) -> u32 {
    match audience {
        ExternalAudience::All => 0,
        ExternalAudience::ContactsOnly => 1,
        ExternalAudience::None => 2,
    }
}

fn audience_from_index(index: u32) -> ExternalAudience {
    match index {
        1 => ExternalAudience::ContactsOnly,
        2 => ExternalAudience::None,
        _ => ExternalAudience::All,
    }
}

fn message_text(view: &gtk::TextView) -> String {
    let buffer = view.buffer();
    buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).trim().to_string()
}

fn set_message_text(view: &gtk::TextView, text: &str) {
    view.buffer().set_text(text);
}

/// Keeps a caption label in step with its assignment without churn.
fn set_label_if_needed(label: &gtk::Label, text: &str) {
    if label.label() != text {
        label.set_label(text);
    }
}

/// A row with a leading label and a widget (the rule editor's shape).
fn labeled_row(label: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    let label = gtk::Label::builder().label(label).width_chars(9).halign(gtk::Align::Start).build();
    widget.set_hexpand(true);
    row.append(&label);
    row.append(widget);
    row
}

/// A labeled switch row.
fn switch_row(label: &str, switch: &gtk::Switch) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    let text = gtk::Label::builder().label(label).halign(gtk::Align::Start).hexpand(true).build();
    row.append(&text);
    row.append(switch);
    row
}

/// A titled section around `content`.
fn section(title: &str, content: &gtk::Box) -> gtk::Box {
    let outer = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    let label = gtk::Label::builder().label(title).halign(gtk::Align::Start).css_classes(["heading"]).build();
    outer.append(&label);
    outer.append(content);
    outer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audience_round_trips_through_the_dropdown() {
        for audience in [ExternalAudience::All, ExternalAudience::ContactsOnly, ExternalAudience::None] {
            assert_eq!(audience_from_index(audience_index(audience)), audience);
        }
        assert_eq!(audience_from_index(9), ExternalAudience::All);
    }

    #[test]
    fn default_start_is_the_next_whole_hour() {
        let start = default_start();
        let local = crate::calendar_tz::display_zone().to_local(start);
        assert_eq!(local.minute(), 0, "starts on the hour");
        assert_eq!(local.second(), 0);
    }
}
