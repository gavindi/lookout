//! The undo stack, send/schedule feedback, the outbox UI and the draft banner.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{AccountId, MailboxId, MailboxRole};
use lookout_mail::session::AccountCommand;

use crate::worker::Worker;

use super::*;

/// How long an Undo toast - and its stack entry - stays live. Matches the
/// short window Outlook gives to reverse a mistaken action; once it elapses
/// the entry is pruned and Ctrl+Z moves on to the previous one.
pub(crate) const UNDO_TOAST_TIMEOUT_SECS: u32 = 8;

/// Upper bound on the retained undo history. Each toast times out on its own,
/// but a burst of actions shouldn't grow the stack without limit before those
/// timeouts fire.
pub(crate) const MAX_UNDO_ENTRIES: usize = 16;

/// One reversible user action (OUTLOOK_PARITY.md item 0.4). `description` is
/// the toast wording ("1 item deleted"); `inverse` are the commands that
/// reverse it, replayed in order by `apply_undo`.
pub(crate) struct UndoEntry {
    pub(crate) id: u64,
    pub(crate) description: String,
    pub(crate) inverse: Vec<AccountCommand>,
}

/// The mailbox a command acts on, used to route its inverse back to the right
/// account. `None` for commands with no mailbox (`SendMessage` and
/// `CancelOutbox`: undo send is the outbox `not_before` delay and the
/// "Sending" toast's own Undo, not this stack).
pub(crate) fn command_mailbox(command: &AccountCommand) -> Option<&MailboxId> {
    match command {
        AccountCommand::RevertMove { moved_to: mailbox, .. }
        | AccountCommand::RevertCopy { copied_to: mailbox, .. }
        | AccountCommand::CopyMessagesTo { mailbox, .. }
        | AccountCommand::UnsnoozeMessages { mailbox, .. }
        | AccountCommand::WakeSnoozed { mailbox, .. }
        | AccountCommand::MoveMessage { mailbox, .. }
        | AccountCommand::MoveMessages { mailbox, .. }
        | AccountCommand::MoveMessagesTo { mailbox, .. }
        | AccountCommand::SnoozeMessage { mailbox, .. }
        | AccountCommand::SnoozeMessages { mailbox, .. }
        | AccountCommand::StoreFlags { mailbox, .. }
        | AccountCommand::StoreFlagsMany { mailbox, .. }
        | AccountCommand::StoreKeywords { mailbox, .. }
        | AccountCommand::StoreKeywordsMany { mailbox, .. }
        | AccountCommand::SetFollowUp { mailbox, .. }
        | AccountCommand::SetImportance { mailbox, .. }
        | AccountCommand::SetFocused { mailbox, .. } => Some(mailbox),
        _ => None,
    }
}

/// The id of `account_id`'s folder carrying `role`, if the account has one.
pub(crate) fn role_mailbox_id(state: &Rc<RefCell<UiState>>, account_id: &AccountId, role: MailboxRole) -> Option<MailboxId> {
    state.borrow().accounts.get(account_id)?.folders.iter().find(|m| m.role == role).map(|m| m.id.clone())
}

/// The Undo-toast wording for a role move (§6.2 / UX-17: "1 item deleted",
/// "2 items archived", …).
pub(crate) fn undo_move_description(role: MailboxRole, count: usize) -> String {
    let noun = if count == 1 { "item" } else { "items" };
    match role {
        MailboxRole::Trash => format!("{count} {noun} deleted"),
        MailboxRole::Archive => format!("{count} {noun} archived"),
        MailboxRole::Junk => format!("{count} {noun} reported as junk"),
        _ => format!("{count} {noun} moved"),
    }
}

/// Replays an entry's inverse commands, each routed to the account owning its
/// mailbox, then shows a brief confirmation.
pub(crate) fn apply_undo(state: &Rc<RefCell<UiState>>, entry: UndoEntry) {
    let UndoEntry { description, inverse, .. } = entry;
    for command in inverse {
        let Some(account_id) = command_mailbox(&command).and_then(mailbox_account_id) else {
            continue;
        };
        let cmd_tx = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone());
        if let Some(cmd_tx) = cmd_tx {
            let _ = cmd_tx.send_blocking(command);
        }
    }
    if let Some(overlay) = &state.borrow().toast_overlay {
        overlay.add_toast(adw::Toast::new(&format!("Undone: {description}")));
    }
}

/// Builds an Undo toast whose button applies exactly this entry (not whatever
/// happens to be newest by the time it's clicked). A no-op when there's no
/// overlay (tests / headless), where no widget may be constructed.
pub(crate) fn show_undo_toast(state: &Rc<RefCell<UiState>>, id: u64, description: &str) {
    let Some(overlay) = state.borrow().toast_overlay.clone() else { return };
    let toast = adw::Toast::new(description);
    toast.set_button_label(Some("Undo"));
    toast.set_timeout(UNDO_TOAST_TIMEOUT_SECS);
    {
        let state = state.clone();
        toast.connect_button_clicked(move |_| {
            if let Some(entry) = take_undo(&state, id) {
                apply_undo(&state, entry);
            }
        });
    }
    {
        let state = state.clone();
        toast.connect_dismissed(move |_| {
            take_undo(&state, id);
        });
    }
    overlay.add_toast(toast);
}

/// A composer send that hasn't gone out yet (item 1.13): its toast, and the
/// message itself so Undo can reopen it in the composer.
pub(crate) struct PendingSend {
    pub(crate) account_id: AccountId,
    pub(crate) client_token: u64,
    /// The outbox row, once `OutboxQueued` has named it.
    pub(crate) outbox_id: Option<i64>,
    pub(crate) message: lookout_mail::ComposedMessage,
    pub(crate) toast: adw::Toast,
    /// Undo was clicked; the `CancelOutbox` goes out as soon as the row id
    /// is known (at once if it already is).
    pub(crate) undo_requested: bool,
    /// A scheduled send (item 1.14): tracked only while its "Scheduled for
    /// …" toast offers Undo, so a send going out hours later raises no
    /// "Message sent" toast, and never counted as an undo-send in flight.
    pub(crate) scheduled: bool,
}

/// The "Sending: <subject>" toast title, markup-escaped (toast titles are
/// Pango markup, and a subject may contain `&` or `<`).
pub(crate) fn sending_toast_title(subject: &str) -> String {
    let subject = if subject.trim().is_empty() { "(No subject)" } else { subject };
    glib::markup_escape_text(&format!("Sending: {subject}")).to_string()
}

/// The `on_send_started` behind every composer's Send: shows the persistent
/// "Sending: <subject>" toast and records the send in `pending_sends`. With
/// an undo-send delay configured (Config → Mail → "Undo send"), the toast
/// offers Undo for that long and the returned `not_before` holds the message
/// in the outbox until then; afterwards the toast loses its button and stays
/// until the send completes. Returns the `SendMessage`'s `(not_before,
/// client_token)`.
pub(crate) fn start_send_feedback(
    state: &Rc<RefCell<UiState>>,
    account_id: Option<AccountId>,
    message: &lookout_mail::ComposedMessage,
    schedule: Option<chrono::DateTime<chrono::Utc>>,
) -> (Option<chrono::DateTime<chrono::Utc>>, Option<u64>) {
    if let Some(at) = schedule {
        return start_schedule_feedback(state, account_id, message, at);
    }
    let toast = adw::Toast::new(&sending_toast_title(&message.subject));
    toast.set_timeout(0);
    let Some(account_id) = account_id else {
        // Nothing to tie it to (no account events will retract it), so let
        // it time out normally.
        toast.set_timeout(5);
        if let Some(overlay) = state.borrow().toast_overlay.clone() {
            overlay.add_toast(toast);
        }
        return (None, None);
    };
    let delay = crate::settings::undo_send_delay(state.borrow().settings.get_int(crate::settings::MAIL_UNDO_SEND_SECONDS));
    let client_token = {
        let mut st = state.borrow_mut();
        let token = st.next_send_token;
        st.next_send_token += 1;
        token
    };
    if delay > 0 {
        toast.set_button_label(Some("Undo"));
        // Shown straight away, ahead of any toast already up, so Undo is
        // reachable for the whole delay.
        toast.set_priority(adw::ToastPriority::High);
        let state = state.clone();
        toast.connect_button_clicked(move |_| request_undo_send(&state, client_token));
    }
    if let Some(overlay) = state.borrow().toast_overlay.clone() {
        overlay.add_toast(toast.clone());
    }
    state.borrow_mut().pending_sends.push(PendingSend {
        account_id,
        client_token,
        outbox_id: None,
        message: message.clone(),
        toast,
        undo_requested: false,
        scheduled: false,
    });
    if delay <= 0 {
        return (None, Some(client_token));
    }
    {
        let state = state.clone();
        glib::timeout_add_seconds_local_once(delay as u32, move || end_undo_window(&state, client_token));
    }
    (Some(chrono::Utc::now() + chrono::Duration::seconds(delay.into())), Some(client_token))
}

/// `start_send_feedback` for a scheduled send (item 1.14): a "Scheduled for
/// Tue 08:00" toast with Undo, which withdraws the message from the outbox
/// and reopens it in the composer. The send is tracked in `pending_sends`
/// only while that toast is up. Returns the scheduled time as `not_before`.
pub(crate) fn start_schedule_feedback(
    state: &Rc<RefCell<UiState>>,
    account_id: Option<AccountId>,
    message: &lookout_mail::ComposedMessage,
    at: chrono::DateTime<chrono::Utc>,
) -> (Option<chrono::DateTime<chrono::Utc>>, Option<u64>) {
    let now = chrono::Local::now();
    let toast = adw::Toast::new(&format!(
        "Scheduled for {}",
        crate::datetime_picker::format_short_time(&at.with_timezone(&chrono::Local), &now)
    ));
    let Some(account_id) = account_id else {
        if let Some(overlay) = state.borrow().toast_overlay.clone() {
            overlay.add_toast(toast);
        }
        return (Some(at), None);
    };
    let client_token = {
        let mut st = state.borrow_mut();
        let token = st.next_send_token;
        st.next_send_token += 1;
        token
    };
    toast.set_button_label(Some("Undo"));
    toast.set_timeout(UNDO_TOAST_TIMEOUT_SECS);
    {
        let state = state.clone();
        toast.connect_button_clicked(move |_| request_undo_send(&state, client_token));
    }
    {
        // Stop tracking once the toast goes - unless Undo was clicked, in
        // which case `OutboxCancelled` still needs the entry to reopen it.
        let state = state.clone();
        toast.connect_dismissed(move |_| {
            state.borrow_mut().pending_sends.retain(|p| p.client_token != client_token || p.undo_requested);
        });
    }
    if let Some(overlay) = state.borrow().toast_overlay.clone() {
        overlay.add_toast(toast.clone());
    }
    state.borrow_mut().pending_sends.push(PendingSend {
        account_id,
        client_token,
        outbox_id: None,
        message: message.clone(),
        toast,
        undo_requested: false,
        scheduled: true,
    });
    (Some(at), Some(client_token))
}

/// The undo-send delay ran out without Undo: swap the toast for a plain
/// persistent "Sending: <subject>" one (an `adw::Toast`'s button can't be
/// taken away once shown) that stays until the send completes.
pub(crate) fn end_undo_window(state: &Rc<RefCell<UiState>>, client_token: u64) {
    let overlay = state.borrow().toast_overlay.clone();
    let mut st = state.borrow_mut();
    let Some(pending) = st.pending_sends.iter_mut().find(|p| p.client_token == client_token && !p.undo_requested) else {
        return;
    };
    let toast = adw::Toast::new(&sending_toast_title(&pending.message.subject));
    toast.set_timeout(0);
    let previous = std::mem::replace(&mut pending.toast, toast.clone());
    drop(st);
    previous.dismiss();
    if let Some(overlay) = overlay {
        overlay.add_toast(toast);
    }
}

/// The "Sending" toast's Undo: withdraw the message from the outbox. The
/// toast dismisses itself on click; the composer reopens when the account
/// confirms with `OutboxCancelled`.
pub(crate) fn request_undo_send(state: &Rc<RefCell<UiState>>, client_token: u64) {
    let (account_id, outbox_id) = {
        let mut st = state.borrow_mut();
        let Some(pending) = st.pending_sends.iter_mut().find(|p| p.client_token == client_token) else {
            return;
        };
        pending.undo_requested = true;
        (pending.account_id.clone(), pending.outbox_id)
    };
    if let Some(id) = outbox_id {
        send_cancel_outbox(state, &account_id, id);
    }
}

pub(crate) fn send_cancel_outbox(state: &Rc<RefCell<UiState>>, account_id: &AccountId, id: i64) {
    let cmd_tx = state.borrow().accounts.get(account_id).map(|handle| handle.cmd_tx.clone());
    if let Some(cmd_tx) = cmd_tx {
        let _ = cmd_tx.send_blocking(AccountCommand::CancelOutbox { id });
    }
}

/// `OutboxQueued`: binds the pending send started with `client_token` to its
/// outbox row, sending the deferred `CancelOutbox` if Undo was clicked first.
pub(crate) fn bind_pending_send(state: &Rc<RefCell<UiState>>, account_id: &AccountId, client_token: u64, id: i64) {
    let undo = {
        let mut st = state.borrow_mut();
        let Some(pending) = st.pending_sends.iter_mut().find(|p| p.client_token == client_token && p.account_id == *account_id) else {
            return;
        };
        pending.outbox_id = Some(id);
        pending.undo_requested
    };
    if undo {
        send_cancel_outbox(state, account_id, id);
    }
}

/// Removes and returns the pending send for an outbox event: by
/// `client_token` when the event carries one (a send that failed to queue),
/// else by outbox row id. Its toast is dismissed.
pub(crate) fn take_pending_send(state: &Rc<RefCell<UiState>>, account_id: &AccountId, id: i64, client_token: Option<u64>) -> Option<PendingSend> {
    let mut st = state.borrow_mut();
    let position = st.pending_sends.iter().position(|p| {
        p.account_id == *account_id
            && match client_token {
                Some(token) => p.client_token == token,
                None => p.outbox_id == Some(id),
            }
    })?;
    let pending = st.pending_sends.remove(position);
    drop(st);
    pending.toast.dismiss();
    Some(pending)
}

/// Undo send landed (`OutboxCancelled`): reopen the message in the composer
/// exactly as it was about to go out, as Outlook does. No signature is
/// appended (the body already has one), and it opens rich exactly when it
/// was sent with an HTML body.
pub(crate) fn reopen_undone_send(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, reading_stack: &gtk::Stack, account_id: &AccountId, message: &lookout_mail::ComposedMessage) {
    reopen_withdrawn_send(state, worker, reading_stack, account_id, crate::compose::build_resend_prefill(message));
}

/// Opens a send withdrawn from the outbox (undo send, or schedule send's
/// "Cancel send"/"Edit", item 1.14) in the composer from its `prefill`.
pub(crate) fn reopen_withdrawn_send(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
    account_id: &AccountId,
    prefill: crate::compose::ComposePrefill,
) {
    let cmd_tx = state.borrow().accounts.get(account_id).map(|handle| handle.cmd_tx.clone());
    let Some(cmd_tx) = cmd_tx else { return };
    let rich_text_default = prefill.html_body.is_some();
    let title = prefill
        .subject
        .clone()
        .filter(|subject| !subject.trim().is_empty())
        .unwrap_or_else(|| "New message".to_string());
    show_composer_in_reading_pane(state, worker, reading_stack, &title, cmd_tx, prefill, rich_text_default, Some(account_id.clone()), None);
}

/// Sends `command` on `account_id`'s background channel, if the account is
/// still connected.
pub(crate) fn send_account_command(state: &Rc<RefCell<UiState>>, account_id: &AccountId, command: AccountCommand) {
    let cmd_tx = state.borrow().accounts.get(account_id).map(|handle| handle.cmd_tx.clone());
    if let Some(cmd_tx) = cmd_tx {
        let _ = cmd_tx.send_blocking(command);
    }
}

/// The outbox rows the banner and the "Scheduled messages" dialog list (item
/// 1.14), per account in a stable order: everything except a send whose own
/// "Sending: …" toast is still up (undo send, item 1.13) - that toast already
/// covers it, and listing it too would flash the banner on every send.
pub(crate) fn listed_outbox_items(st: &UiState) -> Vec<(AccountId, Vec<lookout_mail::session::OutboxItem>)> {
    let mut accounts: Vec<&AccountId> = st.outbox.keys().collect();
    accounts.sort_by(|a, b| a.0.cmp(&b.0));
    accounts
        .into_iter()
        .filter_map(|account_id| {
            let items: Vec<_> = st.outbox[account_id]
                .iter()
                .filter(|item| !st.pending_sends.iter().any(|p| !p.scheduled && p.account_id == *account_id && p.outbox_id == Some(item.id)))
                .cloned()
                .collect();
            (!items.is_empty()).then(|| (account_id.clone(), items))
        })
        .collect()
}

/// The outbox banner's title (item 1.14), or `None` when there's nothing to
/// list: failed sends first, since they need the user; then "scheduled"
/// when everything is waiting for a future time, else "waiting to send"
/// (offline, or retrying after an error).
pub(crate) fn outbox_banner_text(items: &[&lookout_mail::session::OutboxItem], now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    let failed = items.iter().filter(|item| item.failed).count();
    Some(if failed > 0 {
        plural(failed, "message couldn't be sent", "messages couldn't be sent")
    } else if items.iter().all(|item| item.not_before > now) {
        plural(items.len(), "scheduled message", "scheduled messages")
    } else {
        plural(items.len(), "message waiting to send", "messages waiting to send")
    })
}

/// Repaints the outbox banner and, if it's open, the "Scheduled messages"
/// dialog from `UiState::outbox` (item 1.14). An open dialog with nothing
/// left to list closes itself.
pub(crate) fn refresh_outbox_ui(state: &Rc<RefCell<UiState>>) {
    let (banner, dialog, title, empty) = {
        let st = state.borrow();
        let listed = listed_outbox_items(&st);
        let items: Vec<_> = listed.iter().flat_map(|(_, items)| items.iter()).collect();
        let title = outbox_banner_text(&items, chrono::Utc::now());
        (st.outbox_banner.clone(), st.outbox_dialog.clone(), title, listed.is_empty())
    };
    if let Some(banner) = banner {
        if let Some(title) = &title {
            banner.set_title(&glib::markup_escape_text(title));
        }
        banner.set_revealed(title.is_some());
    }
    if let Some((dialog, list)) = dialog {
        if empty {
            dialog.close();
        } else {
            fill_outbox_dialog(state, &list);
        }
    }
}

/// Opens the "Scheduled messages" dialog (item 1.14) over `parent`.
pub(crate) fn present_outbox_dialog(state: &Rc<RefCell<UiState>>, parent: &impl IsA<gtk::Widget>) {
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    let dialog = adw::Dialog::builder().title("Scheduled messages").content_width(520).child(&toolbar).build();
    {
        let state = state.clone();
        dialog.connect_closed(move |_| state.borrow_mut().outbox_dialog = None);
    }
    state.borrow_mut().outbox_dialog = Some((dialog.clone(), list.clone()));
    fill_outbox_dialog(state, &list);
    dialog.present(Some(parent));
}

/// (Re)builds the dialog's rows: one group per account, one row per message
/// with "Send now" (or "Retry" for a failed one) and "Edit", which withdraws
/// it from the outbox and reopens it in the composer (on `OutboxCancelled`).
pub(crate) fn fill_outbox_dialog(state: &Rc<RefCell<UiState>>, list: &gtk::Box) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let (listed, emails) = {
        let st = state.borrow();
        let listed = listed_outbox_items(&st);
        let emails: HashMap<AccountId, String> = listed
            .iter()
            .map(|(id, _)| (id.clone(), st.accounts.get(id).map(|h| h.email.clone()).unwrap_or_else(|| id.0.clone())))
            .collect();
        (listed, emails)
    };
    let now_utc = chrono::Utc::now();
    let now = chrono::Local::now();
    for (account_id, items) in listed {
        let group = adw::PreferencesGroup::builder().title(glib::markup_escape_text(&emails[&account_id]).as_str()).build();
        // The group heading is an account address: content, not chrome.
        crate::privacy::mark_content_heading(&group);
        for item in items {
            let status = if item.failed {
                "Couldn't send".to_string()
            } else if item.not_before > now_utc {
                format!(
                    "Scheduled for {}",
                    crate::datetime_picker::format_short_time(&item.not_before.with_timezone(&chrono::Local), &now)
                )
            } else {
                "Waiting to send".to_string()
            };
            let to = if item.recipients.is_empty() {
                String::new()
            } else {
                format!("To: {} · ", item.recipients.join(", "))
            };
            let subject = if item.subject.trim().is_empty() { "(No subject)" } else { item.subject.as_str() };
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(subject).as_str())
                .subtitle(glib::markup_escape_text(&format!("{to}{status}")).as_str())
                .subtitle_lines(2)
                .build();
            crate::privacy::mark_content(&row);
            let send_now = gtk::Button::builder()
                .label(if item.failed { "Retry" } else { "Send now" })
                .valign(gtk::Align::Center)
                .build();
            {
                let state = state.clone();
                let account_id = account_id.clone();
                let id = item.id;
                send_now.connect_clicked(move |_| {
                    send_account_command(
                        &state,
                        &account_id,
                        AccountCommand::RescheduleOutbox {
                            id,
                            not_before: chrono::Utc::now(),
                        },
                    )
                });
            }
            let edit = gtk::Button::builder().label("Edit").valign(gtk::Align::Center).build();
            {
                let state = state.clone();
                let account_id = account_id.clone();
                let id = item.id;
                edit.connect_clicked(move |_| {
                    send_account_command(&state, &account_id, AccountCommand::CancelOutbox { id });
                    let dialog = state.borrow().outbox_dialog.as_ref().map(|(dialog, _)| dialog.clone());
                    if let Some(dialog) = dialog {
                        dialog.close();
                    }
                });
            }
            row.add_suffix(&send_now);
            row.add_suffix(&edit);
            group.add(&row);
        }
        list.append(&group);
    }
}

/// The outbox row a saved draft is waiting to go out as (item 1.14): the
/// scheduled send whose Drafts copy has `message_id` in `mailbox`'s account.
pub(crate) fn scheduled_item_for_draft(st: &UiState, mailbox: &MailboxId, message_id: &str) -> Option<(AccountId, lookout_mail::session::OutboxItem)> {
    let account_id = mailbox_account_id(mailbox)?;
    let item = st
        .outbox
        .get(&account_id)?
        .iter()
        .find(|item| item.draft_message_id.as_deref() == Some(message_id))?
        .clone();
    Some((account_id, item))
}

/// Sets the reading pane's draft banner (items 1.12 / 1.14): hidden unless
/// the rendered message is a draft; "Scheduled to send Tue 08:00" with
/// "Cancel send" when that draft is a scheduled send waiting to go out (or
/// "Waiting to send" / "Couldn't send"), else "This is a draft" with "Edit".
pub(crate) fn refresh_draft_banner(state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, is_rendered_draft: bool) {
    let Some(banner) = find_named_child(reading_stack, "draft-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) else {
        return;
    };
    let scheduled = {
        let st = state.borrow();
        match (&st.rendered_draft, st.rendered_draft_message_id.as_deref()) {
            (Some((mailbox, _)), Some(id)) if is_rendered_draft => scheduled_item_for_draft(&st, mailbox, id).map(|(_, item)| item),
            _ => None,
        }
    };
    match scheduled {
        Some(item) => {
            let title = if item.failed {
                "This message couldn't be sent".to_string()
            } else if item.not_before > chrono::Utc::now() {
                format!(
                    "Scheduled to send {}",
                    crate::datetime_picker::format_short_time(&item.not_before.with_timezone(&chrono::Local), &chrono::Local::now())
                )
            } else {
                "This message is waiting to send".to_string()
            };
            banner.set_title(&title);
            banner.set_button_label(Some("Cancel send"));
        }
        None => {
            banner.set_title("This is a draft");
            banner.set_button_label(Some("Edit"));
        }
    }
    banner.set_revealed(is_rendered_draft);
}
