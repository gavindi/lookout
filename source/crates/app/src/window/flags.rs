//! Follow-up flag and info-banner UI for the message list and reading pane.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{EmailSummary, MailboxId, Uid};
use lookout_mail::session::AccountCommand;

use crate::message_list::MessageListModel;

use super::*;

/// The hidden marker line appended to a "Create task from message" task's
/// Notes, recording which email spawned it - the only channel that reliably
/// round-trips through every task backend (CalDAV's `DESCRIPTION` and Google
/// Tasks' `notes` both preserve free text as-is, but Google Tasks drops
/// `categories` entirely - see `task_to_write` - so a tag stored there
/// wouldn't survive a Google Tasks round trip).
pub(crate) fn task_email_marker(message_id: &str) -> String {
    format!("Lookout-Message-Id: {message_id}")
}

/// The flag icon: filled when `flagged`, outline otherwise. Shared by the
/// toolbar's Flag button and the message row's hover button.
pub(crate) fn follow_up_icon_name(flagged: bool) -> &'static str {
    if flagged {
        themed_icon_name(&["mail-flag-symbolic", "flag-filled-symbolic", "mail-mark-important-symbolic"])
    } else {
        themed_icon_name(&["flag-outline-thin-symbolic", "flag-outline-symbolic", "mail-mark-important-symbolic"])
    }
}

/// The toolbar Flag button's icon and tooltip for the selection: filled
/// "Clear flag" when any selected message has an open follow-up flag (what a
/// click then does), outline "Flag" otherwise.
pub(crate) fn refresh_flag_button(icon: &crate::ribbon::RibbonIcon, message_list: &MessageListModel) {
    let flagged = message_list.selected_summaries().iter().any(EmailSummary::is_flagged_for_follow_up);
    icon.set_icon_name(follow_up_icon_name(flagged));
    icon.set_tooltip_text(Some(if flagged { "Clear flag" } else { "Flag" }));
}

/// The row's hover Flag button for `flag`, mirroring `refresh_flag_button`.
pub(crate) fn set_flag_button_state(button: &gtk::Button, flag: Option<&lookout_core::FollowUpFlag>) {
    let flagged = flag.is_some_and(|flag| flag.status == lookout_core::FlagStatus::Flagged);
    button.set_icon_name(follow_up_icon_name(flagged));
    button.set_tooltip_text(Some(if flagged { "Clear flag" } else { "Flag" }));
}

/// Shows a follow-up flag on a list row (item 1.16): a filled flag icon and,
/// when `label` is given and the flag has a due date, its due label - both in
/// the error colour once overdue - or a check icon for a completed flag.
/// Hidden when there's no flag, so an unflagged row looks as before.
pub(crate) fn apply_flag_indicator(icon: &gtk::Image, label: Option<&gtk::Label>, flag: Option<&lookout_core::FollowUpFlag>) {
    let today = crate::calendar_tz::display_zone().today();
    let status = flag.map(|flag| flag.status).unwrap_or_default();
    let due = flag.and_then(crate::follow_up::due_date);
    let overdue = status == lookout_core::FlagStatus::Flagged && due.is_some_and(|due| due < today);
    match status {
        lookout_core::FlagStatus::NotFlagged => icon.set_visible(false),
        lookout_core::FlagStatus::Flagged => {
            icon.set_icon_name(Some(follow_up_icon_name(true)));
            icon.set_visible(true);
        }
        lookout_core::FlagStatus::Complete => {
            icon.set_icon_name(Some("object-select-symbolic"));
            icon.set_visible(true);
        }
    }
    icon.set_tooltip_text(flag.map(|flag| crate::follow_up::banner_text(flag, today)).as_deref());
    if overdue {
        icon.add_css_class("error");
    } else {
        icon.remove_css_class("error");
    }
    if let Some(label) = label {
        let text = due
            .filter(|_| status == lookout_core::FlagStatus::Flagged)
            .map(|due| crate::follow_up::due_label(due, today));
        label.set_label(text.as_deref().unwrap_or(""));
        label.set_visible(text.is_some());
        if overdue {
            label.add_css_class("error");
        } else {
            label.remove_css_class("error");
        }
    }
}

/// The Flag button's main action: clears the flag when any of `summaries`
/// has an open one, else flags them all with no due date.
pub(crate) fn toggle_follow_up(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, summaries: Vec<EmailSummary>) {
    let flag = if summaries.iter().any(EmailSummary::is_flagged_for_follow_up) {
        crate::follow_up::cleared()
    } else {
        crate::follow_up::flagged(None)
    };
    set_follow_up(state, message_list, summaries, flag);
}

/// Sets `flag` on `summaries` (item 1.16): shows it at once, sends one
/// `SetFollowUp` per mailbox, and records an undo that restores each
/// message's previous flag (grouped by that previous state).
pub(crate) fn set_follow_up(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, summaries: Vec<EmailSummary>, flag: lookout_core::FollowUpFlag) {
    if summaries.is_empty() {
        return;
    }
    let mut by_mailbox: Vec<(MailboxId, Vec<Uid>)> = Vec::new();
    let mut previous: Vec<(MailboxId, lookout_core::FollowUpFlag, Vec<Uid>)> = Vec::new();
    for summary in &summaries {
        match by_mailbox.iter_mut().find(|(mailbox, _)| *mailbox == summary.mailbox) {
            Some((_, uids)) => uids.push(summary.uid),
            None => by_mailbox.push((summary.mailbox.clone(), vec![summary.uid])),
        }
        let before = summary.follow_up().unwrap_or_default();
        match previous.iter_mut().find(|(mailbox, flag, _)| *mailbox == summary.mailbox && *flag == before) {
            Some((_, _, uids)) => uids.push(summary.uid),
            None => previous.push((summary.mailbox.clone(), before, vec![summary.uid])),
        }
    }
    for (mailbox, uids) in by_mailbox {
        let Some(account_id) = mailbox_account_id(&mailbox) else { continue };
        let Some(cmd_tx) = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone()) else {
            continue;
        };
        optimistic_set_follow_up(state, message_list, &mailbox, &uids, &flag);
        let _ = cmd_tx.send_blocking(AccountCommand::SetFollowUp {
            mailbox,
            uids,
            flag: flag.clone(),
        });
    }
    let inverse = previous
        .into_iter()
        .map(|(mailbox, flag, uids)| AccountCommand::SetFollowUp { mailbox, uids, flag })
        .collect();
    let today = crate::calendar_tz::display_zone().today();
    record_undo(state, &crate::follow_up::action_label(&flag, today), inverse);
}

/// Shows `flag` on `mailbox`/`uids` in the list (and the unified view's
/// snapshot) before the session confirms it. Not stashed for rollback: the
/// session re-emits the folder from its cache after every `SetFollowUp`,
/// failed or not, which puts the true state back.
pub(crate) fn optimistic_set_follow_up(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid], flag: &lookout_core::FollowUpFlag) {
    let keywords = lookout_core::follow_up_keywords(flag);
    let patch = |m: &mut EmailSummary| {
        m.keywords.retain(|k| !lookout_core::is_flag_keyword(k));
        m.keywords.extend(keywords.iter().cloned());
    };
    let mut changed = false;
    let all: Vec<EmailSummary> = message_list
        .all_messages()
        .into_iter()
        .map(|mut m| {
            if m.mailbox == *mailbox && uids.contains(&m.uid) {
                patch(&mut m);
                changed = true;
            }
            m
        })
        .collect();
    if changed {
        let (key, descending) = current_sort(state);
        message_list.repopulate(all, key, descending);
    }
    if let Some(snapshot) = state.borrow_mut().unified_snapshots.get_mut(mailbox) {
        for m in snapshot.iter_mut().filter(|m| uids.contains(&m.uid)) {
            patch(m);
        }
    }
}

/// Shows or hides the reading pane's "Follow up. Due by …" bar for the
/// message on it, from that message's current summary in the list.
pub(crate) fn refresh_follow_up_banner(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, reading_stack: &gtk::Stack) {
    let rendered = state.borrow().rendered_message.clone();
    let flag = rendered.as_ref().and_then(|(mailbox, uid)| {
        let summary = message_list.all_messages().into_iter().find(|m| m.mailbox == *mailbox && m.uid == *uid)?;
        summary.follow_up().map(|flag| (mailbox.clone(), *uid, flag))
    });
    show_follow_up_banner(state, reading_stack, flag);
}

/// Reveals the follow-up bar for `flag` (or hides it for `None`), and keeps
/// `rendered_follow_up` in step for its button.
pub(crate) fn show_follow_up_banner(state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, flag: Option<(MailboxId, Uid, lookout_core::FollowUpFlag)>) {
    let Some(banner) = find_named_child(reading_stack, "follow-up-banner").and_downcast::<adw::Banner>() else {
        return;
    };
    match &flag {
        Some((_, _, follow_up)) => {
            let today = crate::calendar_tz::display_zone().today();
            banner.set_title(&glib::markup_escape_text(&crate::follow_up::banner_text(follow_up, today)));
            banner.set_button_label(Some(if follow_up.status == lookout_core::FlagStatus::Complete {
                "Clear flag"
            } else {
                "Mark complete"
            }));
            banner.set_revealed(true);
        }
        None => banner.set_revealed(false),
    }
    state.borrow_mut().rendered_follow_up = flag;
}

/// The Flag ▾ menu (item 1.16): Outlook's due-date presets for today (each
/// with its date), "Custom date…", then Mark complete and Clear flag.
/// Rebuilt every time it opens, so "Today" never goes stale and the presets
/// follow the calendar's current week start and work days.
pub(crate) fn build_flag_popover(state: &Rc<RefCell<UiState>>, on_pick: Rc<dyn Fn(lookout_core::FollowUpFlag)>) -> gtk::Popover {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let popover = gtk::Popover::builder().child(&list).build();
    popover.add_css_class("menu");
    let state = state.clone();
    let fill = {
        let list = list.clone();
        let popover = popover.clone();
        move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let heading = gtk::Label::builder()
                .label("Flag")
                .css_classes(["heading"])
                .xalign(0.0)
                .margin_start(8)
                .margin_top(4)
                .margin_bottom(4)
                .build();
            crate::privacy::mark_ui(&heading);
            list.append(&heading);
            let item = |label: &str, detail: Option<String>, flag: lookout_core::FollowUpFlag| {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                row.append(&gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build());
                if let Some(detail) = detail {
                    row.append(&gtk::Label::builder().label(detail).css_classes(["dim-label"]).build());
                }
                let button = gtk::Button::builder().child(&row).css_classes(["flat"]).build();
                let on_pick = on_pick.clone();
                let popover = popover.clone();
                button.connect_clicked(move |_| {
                    popover.popdown();
                    on_pick(flag.clone());
                });
                button
            };
            let today = crate::calendar_tz::display_zone().today();
            let prefs = state.borrow().settings.calendar_preferences();
            for preset in crate::follow_up::flag_presets(today, prefs.week_start, &prefs.work_days) {
                let detail = preset.due.map(|due| due.format("%a %-d %b").to_string());
                list.append(&item(preset.name, detail, crate::follow_up::flagged(preset.due)));
            }
            let custom = gtk::Button::builder()
                .child(&gtk::Label::builder().label("Custom date…").xalign(0.0).build())
                .css_classes(["flat"])
                .build();
            {
                let on_pick = on_pick.clone();
                let popover = popover.clone();
                custom.connect_clicked(move |_| {
                    popover.popdown();
                    let on_pick = on_pick.clone();
                    crate::datetime_picker::present_date_dialog(
                        popover.parent().as_ref(),
                        "Flag for follow-up",
                        "Due date",
                        "Flag",
                        today,
                        Rc::new(move |due| on_pick(crate::follow_up::flagged(Some(due)))),
                    );
                });
            }
            list.append(&custom);
            list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            list.append(&item("Mark complete", None, crate::follow_up::completed()));
            list.append(&item("Clear flag", None, crate::follow_up::cleared()));
        }
    };
    fill();
    popover.connect_show(move |_| fill());
    popover
}
