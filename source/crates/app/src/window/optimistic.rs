//! Optimistic local updates (read, flag, pin, removal) with their confirm, restore and reconcile paths.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk::glib;
use lookout_core::{EmailSummary, MailboxId, SystemFlagBit, Uid};
use lookout_mail::session::AccountCommand;

use crate::message_list::{MessageListModel, SelectionKind};

use super::*;

/// Removes `uids` from `mailbox` in the message list immediately, ahead of
/// the server-side move just sent for them actually completing - that move
/// takes several sequential live IMAP round trips (`move_uids_to_path` in
/// `lookout-mail`'s session actor), and waiting for it to finish before
/// hiding the row is the ~2s lag a delete/archive/report used to have. The
/// removed rows are stashed in `state.pending_optimistic_removals` so a
/// matching `AccountEvent::MoveFailed` can restore them; a later
/// `MessagesUpdated` for this mailbox (which the success path always
/// eventually produces) clears the stash instead, since the authoritative
/// sync already reflects reality.
pub(crate) fn optimistic_remove_messages(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid]) {
    let (kept, removed): (Vec<EmailSummary>, Vec<EmailSummary>) = message_list.all_messages().into_iter().partition(|m| !(m.mailbox == *mailbox && uids.contains(&m.uid)));
    if removed.is_empty() {
        return;
    }
    let (key, descending) = current_sort(state);
    message_list.repopulate(kept, key, descending);
    let mut st = state.borrow_mut();
    if let Some(snapshot) = st.unified_snapshots.get_mut(mailbox) {
        snapshot.retain(|m| !uids.contains(&m.uid));
    }
    st.pending_optimistic_removals.entry(mailbox.clone()).or_default().extend(removed);
}

/// Undoes `optimistic_remove_messages` for `mailbox`/`uids` after an
/// `AccountEvent::MoveFailed` - puts the stashed rows back into both the
/// visible list and the unified-view snapshot they were pulled from.
pub(crate) fn restore_optimistic_removals(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid]) {
    let restored: Vec<EmailSummary> = {
        let mut st = state.borrow_mut();
        let Some(pending) = st.pending_optimistic_removals.get_mut(mailbox) else { return };
        let mut restored = Vec::new();
        pending.retain(|m| {
            if uids.contains(&m.uid) {
                restored.push(m.clone());
                false
            } else {
                true
            }
        });
        if pending.is_empty() {
            st.pending_optimistic_removals.remove(mailbox);
        }
        restored
    };
    if restored.is_empty() {
        return;
    }
    if let Some(snapshot) = state.borrow_mut().unified_snapshots.get_mut(mailbox) {
        snapshot.extend(restored.iter().cloned());
    }
    let mut all = message_list.all_messages();
    all.extend(restored);
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
}

/// Reconciles an incoming authoritative `MessagesUpdated` snapshot against
/// optimistic removals still pending for `mailbox`. The account session's
/// IDLE-vs-command race means a resync can land for reasons unrelated to a
/// pending delete, before the delete itself has reached the server - so a
/// uid this client just hid can still show up in `messages`. A pending entry
/// only survives if `messages` still shows that uid (the delete hasn't
/// landed yet); anything `messages` omits is confirmed gone and drops out of
/// the stash. Uids still pending after that are stripped back out of
/// `messages` so the race can't repaint a row the user already deleted.
pub(crate) fn reconcile_optimistic_removals(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, messages: &mut Vec<EmailSummary>) {
    let mut st = state.borrow_mut();
    let Some(pending) = st.pending_optimistic_removals.get_mut(mailbox) else { return };
    let live_uids: HashSet<Uid> = messages.iter().map(|m| m.uid).collect();
    pending.retain(|m| live_uids.contains(&m.uid));
    if pending.is_empty() {
        st.pending_optimistic_removals.remove(mailbox);
        return;
    }
    let still_pending: HashSet<Uid> = st.pending_optimistic_removals[mailbox].iter().map(|m| m.uid).collect();
    drop(st);
    messages.retain(|m| !still_pending.contains(&m.uid));
}

/// Sends the `\Seen` store that marks `uid` read on its account, best-effort
/// (a mailbox with no live account handle is silently skipped). Shared by
/// every mark-as-read mode (item 1.8).
pub(crate) fn mark_message_read(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid) {
    let st = state.borrow();
    if let Some(handle) = mailbox_account_id(mailbox).and_then(|id| st.accounts.get(&id)) {
        let _ = handle.cmd_tx.send_blocking(AccountCommand::StoreFlags {
            mailbox: mailbox.clone(),
            uid,
            add: vec![SystemFlagBit::Seen],
            remove: Vec::new(),
        });
    }
}

/// Cancels any pending "mark as read after a delay" timer (item 1.8). Called
/// on every navigation, so a message left before its delay elapsed isn't
/// marked read later.
pub(crate) fn cancel_pending_mark_read(state: &Rc<RefCell<UiState>>) {
    if let Some((source, _, _)) = state.borrow_mut().pending_mark_read.take() {
        source.remove();
    }
}

/// Marks the message recorded by the "on-selection-change" mode read, now that
/// the selection has moved off it (item 1.8). A no-op when nothing is
/// recorded.
pub(crate) fn flush_mark_read_on_leave(state: &Rc<RefCell<UiState>>) {
    let candidate = state.borrow_mut().mark_read_on_leave.take();
    if let Some((mailbox, uid)) = candidate {
        mark_message_read(state, &mailbox, uid);
    }
}

/// Arms the "mark as read after a delay" timer (item 1.8): `uid` is marked
/// read `delay_secs` later, but only if it's still the selected message then.
/// Replaces any already-pending timer.
pub(crate) fn arm_pending_mark_read(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uid: Uid, delay_secs: u32) {
    cancel_pending_mark_read(state);
    let state_for_timer = state.clone();
    let message_list = message_list.clone();
    let mailbox_for_timer = mailbox.clone();
    let source = glib::timeout_add_local_once(std::time::Duration::from_secs(u64::from(delay_secs.max(1))), move || {
        let state = state_for_timer;
        state.borrow_mut().pending_mark_read = None;
        let still_selected = matches!(
            message_list.selection_kind(),
            SelectionKind::Message(m) if m.mailbox == mailbox_for_timer && m.uid == uid
        );
        if still_selected {
            mark_message_read(&state, &mailbox_for_timer, uid);
        }
    });
    state.borrow_mut().pending_mark_read = Some((source, mailbox.clone(), uid));
}

/// Optimistically flips `SystemFlagBit::Seen` for `uids` in `mailbox` -
/// added when marking read, removed when marking unread - and repaints
/// immediately. The pre-toggle summaries are stashed in
/// `pending_optimistic_flag_changes` so a matching `StoreFlagsFailed` can
/// restore exactly them; a later `MessagesUpdated` for this mailbox clears
/// the stash on its own, since the success path already patched the cache
/// to match before emitting it.
pub(crate) fn optimistic_toggle_read(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid], mark_read: bool) {
    let mut before = Vec::new();
    let all: Vec<EmailSummary> = message_list
        .all_messages()
        .into_iter()
        .map(|mut m| {
            if m.mailbox == *mailbox && uids.contains(&m.uid) {
                before.push(m.clone());
                if mark_read {
                    m.flags.insert(SystemFlagBit::Seen);
                } else {
                    m.flags.remove(&SystemFlagBit::Seen);
                }
            }
            m
        })
        .collect();
    if before.is_empty() {
        return;
    }
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
    let mut st = state.borrow_mut();
    if let Some(snapshot) = st.unified_snapshots.get_mut(mailbox) {
        for m in snapshot.iter_mut().filter(|m| uids.contains(&m.uid)) {
            if mark_read {
                m.flags.insert(SystemFlagBit::Seen);
            } else {
                m.flags.remove(&SystemFlagBit::Seen);
            }
        }
    }
    st.pending_optimistic_flag_changes.entry(mailbox.clone()).or_default().extend(before);
}

/// Undoes `optimistic_toggle_read` for `mailbox`/`uids` after a
/// `StoreFlagsFailed` - puts the stashed pre-toggle summaries back in place
/// (by uid) in both the visible list and the unified-view snapshot.
pub(crate) fn restore_optimistic_flag_changes(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid]) {
    let restored: HashMap<Uid, EmailSummary> = {
        let mut st = state.borrow_mut();
        let Some(pending) = st.pending_optimistic_flag_changes.get_mut(mailbox) else { return };
        let mut restored = HashMap::new();
        pending.retain(|m| {
            if uids.contains(&m.uid) {
                restored.insert(m.uid, m.clone());
                false
            } else {
                true
            }
        });
        if pending.is_empty() {
            st.pending_optimistic_flag_changes.remove(mailbox);
        }
        restored
    };
    if restored.is_empty() {
        return;
    }
    let mut st = state.borrow_mut();
    if let Some(snapshot) = st.unified_snapshots.get_mut(mailbox) {
        for m in snapshot.iter_mut() {
            if let Some(orig) = restored.get(&m.uid) {
                m.flags = orig.flags.clone();
            }
        }
    }
    drop(st);
    let all: Vec<EmailSummary> = message_list.all_messages().into_iter().map(|m| restored.get(&m.uid).cloned().unwrap_or(m)).collect();
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
}

/// The read/unread sibling of `reconcile_optimistic_removals`: an incoming
/// `MessagesUpdated` snapshot can race ahead of the `STORE` a pending
/// `optimistic_toggle_read` is still waiting on, and land with the
/// pre-toggle `Seen` flag. For each uid still in `pending_optimistic_flag_changes`,
/// compares the snapshot's flag against what `message_list` is currently
/// showing (the optimistic value): a match means the server confirmed it -
/// drop the uid from the stash - a mismatch means the snapshot is stale, so
/// `messages` is patched to keep showing the optimistic value instead of
/// flickering back, and the uid stays stashed.
pub(crate) fn reconcile_optimistic_flag_changes(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, messages: &mut [EmailSummary]) {
    let pending_uids: HashSet<Uid> = match state.borrow().pending_optimistic_flag_changes.get(mailbox) {
        Some(pending) if !pending.is_empty() => pending.iter().map(|m| m.uid).collect(),
        _ => return,
    };
    let optimistic_seen: HashMap<Uid, bool> = message_list
        .all_messages()
        .into_iter()
        .filter(|m| m.mailbox == *mailbox && pending_uids.contains(&m.uid))
        .map(|m| (m.uid, m.flags.contains(&SystemFlagBit::Seen)))
        .collect();

    let mut confirmed = HashSet::new();
    for m in messages.iter_mut() {
        let Some(&want_seen) = optimistic_seen.get(&m.uid) else { continue };
        if m.flags.contains(&SystemFlagBit::Seen) == want_seen {
            confirmed.insert(m.uid);
        } else if want_seen {
            m.flags.insert(SystemFlagBit::Seen);
        } else {
            m.flags.remove(&SystemFlagBit::Seen);
        }
    }

    let mut st = state.borrow_mut();
    let Some(pending) = st.pending_optimistic_flag_changes.get_mut(mailbox) else { return };
    pending.retain(|m| !confirmed.contains(&m.uid));
    if pending.is_empty() {
        st.pending_optimistic_flag_changes.remove(mailbox);
    }
}

/// Optimistically flips `SystemFlagBit::Flagged` for `uids` in `mailbox` -
/// added when pinning, removed when unpinning - and repaints immediately,
/// including moving the message into or out of the Pinned group
/// (`message_list.rs`'s `compute_layout`/`splice_pinned_section`) rather
/// than waiting for the next `MessagesUpdated` to confirm it. The pin/unpin
/// sibling of `optimistic_toggle_read`, with its own stash
/// (`pending_optimistic_pinned_changes`) rather than sharing
/// `pending_optimistic_flag_changes` - see that field's doc comment.
pub(crate) fn optimistic_toggle_pinned(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid], pinned: bool) {
    let mut before = Vec::new();
    let all: Vec<EmailSummary> = message_list
        .all_messages()
        .into_iter()
        .map(|mut m| {
            if m.mailbox == *mailbox && uids.contains(&m.uid) {
                before.push(m.clone());
                if pinned {
                    m.flags.insert(SystemFlagBit::Flagged);
                } else {
                    m.flags.remove(&SystemFlagBit::Flagged);
                }
            }
            m
        })
        .collect();
    if before.is_empty() {
        return;
    }
    mirror_pin_to_graph(state, mailbox, &before, pinned);
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
    let mut st = state.borrow_mut();
    if let Some(snapshot) = st.unified_snapshots.get_mut(mailbox) {
        for m in snapshot.iter_mut().filter(|m| uids.contains(&m.uid)) {
            if pinned {
                m.flags.insert(SystemFlagBit::Flagged);
            } else {
                m.flags.remove(&SystemFlagBit::Flagged);
            }
        }
    }
    st.pending_optimistic_pinned_changes.entry(mailbox.clone()).or_default().extend(before);
}

/// Best-effort side channel alongside the IMAP `\Flagged` toggle in
/// `optimistic_toggle_pinned`: on Microsoft 365 accounts that have opted
/// into Graph pin-mirroring (see `AccountHandle::graph_pin`, set up from
/// Config's "Sync pins with Outlook" action), also write `PR_RENEW_TIME` via
/// Graph so Outlook's own pin reflects the change. The single early return
/// below for accounts without a `graph_pin` client is the entire guarantee
/// that every non-opted-in account - which is every account except
/// Microsoft 365 ones with sync explicitly enabled - sees zero behavior
/// change from this feature.
///
/// Fired at optimistic-toggle time rather than gated on `StoreFlagsMany`
/// actually succeeding: this matches the optimistic-UI pattern the rest of
/// this file uses, and avoids threading a new Microsoft-only confirmation
/// event through the protocol-agnostic `crates/mail` session actor. A rare
/// IMAP-side rollback can leave the Graph mirror briefly stale until the
/// next pin action.
pub(crate) fn mirror_pin_to_graph(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, before: &[EmailSummary], pinned: bool) {
    let Some(account_id) = mailbox_account_id(mailbox) else { return };
    let Some(client) = state.borrow().accounts.get(&account_id).and_then(|h| h.graph_pin.clone()) else {
        return;
    };
    for m in before {
        let Some(message_id) = m.message_id.clone() else { continue };
        let client = client.clone();
        let original_date = m.date;
        glib::spawn_future_local(async move {
            if let Err(e) = client.set_pinned(&message_id, original_date, pinned).await {
                match e {
                    // Expected for most Microsoft 365 accounts (sync isn't
                    // enabled) or a message Graph hasn't indexed yet - quiet
                    // on purpose, never surfaced as a pin failure.
                    crate::graph_pin::GraphPinError::ConsentRequired => {
                        tracing::debug!("skipping Outlook pin mirror for {message_id}: Graph pin sync isn't enabled for this account");
                    }
                    crate::graph_pin::GraphPinError::MessageNotFound => {
                        tracing::debug!("skipping Outlook pin mirror for {message_id}: no matching Graph message found");
                    }
                    crate::graph_pin::GraphPinError::Http(msg) => {
                        tracing::warn!("Outlook pin mirror failed for {message_id}: {msg}");
                    }
                }
            }
        });
    }
}

/// Undoes `optimistic_toggle_pinned` for `mailbox`/`uids` after a
/// `StoreFlagsFailed` - puts the stashed pre-toggle summaries back in place
/// (by uid) in both the visible list and the unified-view snapshot.
pub(crate) fn restore_optimistic_pinned_changes(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid]) {
    let restored: HashMap<Uid, EmailSummary> = {
        let mut st = state.borrow_mut();
        let Some(pending) = st.pending_optimistic_pinned_changes.get_mut(mailbox) else { return };
        let mut restored = HashMap::new();
        pending.retain(|m| {
            if uids.contains(&m.uid) {
                restored.insert(m.uid, m.clone());
                false
            } else {
                true
            }
        });
        if pending.is_empty() {
            st.pending_optimistic_pinned_changes.remove(mailbox);
        }
        restored
    };
    if restored.is_empty() {
        return;
    }
    let mut st = state.borrow_mut();
    if let Some(snapshot) = st.unified_snapshots.get_mut(mailbox) {
        for m in snapshot.iter_mut() {
            if let Some(orig) = restored.get(&m.uid) {
                m.flags = orig.flags.clone();
            }
        }
    }
    drop(st);
    let all: Vec<EmailSummary> = message_list.all_messages().into_iter().map(|m| restored.get(&m.uid).cloned().unwrap_or(m)).collect();
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
}

/// The pin/unpin sibling of `reconcile_optimistic_flag_changes`: an
/// incoming `MessagesUpdated` snapshot can race ahead of the `STORE` a
/// pending `optimistic_toggle_pinned` is still waiting on, and land with
/// the pre-toggle `Flagged` state. For each uid still in
/// `pending_optimistic_pinned_changes`, compares the snapshot's flag
/// against what `message_list` is currently showing (the optimistic value):
/// a match means the server confirmed it - drop the uid from the stash - a
/// mismatch means the snapshot is stale, so `messages` is patched to keep
/// showing the optimistic value instead of flickering back, and the uid
/// stays stashed.
pub(crate) fn reconcile_optimistic_pinned_changes(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, messages: &mut [EmailSummary]) {
    let pending_uids: HashSet<Uid> = match state.borrow().pending_optimistic_pinned_changes.get(mailbox) {
        Some(pending) if !pending.is_empty() => pending.iter().map(|m| m.uid).collect(),
        _ => return,
    };
    let optimistic_pinned: HashMap<Uid, bool> = message_list
        .all_messages()
        .into_iter()
        .filter(|m| m.mailbox == *mailbox && pending_uids.contains(&m.uid))
        .map(|m| (m.uid, m.flags.contains(&SystemFlagBit::Flagged)))
        .collect();

    let mut confirmed = HashSet::new();
    for m in messages.iter_mut() {
        let Some(&want_pinned) = optimistic_pinned.get(&m.uid) else { continue };
        if m.flags.contains(&SystemFlagBit::Flagged) == want_pinned {
            confirmed.insert(m.uid);
        } else if want_pinned {
            m.flags.insert(SystemFlagBit::Flagged);
        } else {
            m.flags.remove(&SystemFlagBit::Flagged);
        }
    }

    let mut st = state.borrow_mut();
    let Some(pending) = st.pending_optimistic_pinned_changes.get_mut(mailbox) else { return };
    pending.retain(|m| !confirmed.contains(&m.uid));
    if pending.is_empty() {
        st.pending_optimistic_pinned_changes.remove(mailbox);
    }
}

/// Patches a preview snippet and/or `@mentions` verdict onto matching
/// displayed rows in place - the UI-side half of
/// `AccountEvent::PreviewsFetched` (M-25). Not optimistic (unlike
/// `optimistic_toggle_read` above): the cache write already succeeded before
/// this event was sent, so there's nothing to stash for rollback.
///
/// `unified_snapshots` is kept in sync by the event-loop caller separately,
/// unconditionally on every `PreviewsFetched` regardless of whether this
/// function even runs - a preview must persist there even for a mailbox
/// that isn't currently on screen, whereas this function only has anything
/// to patch when the affected mailbox is actually displayed.
pub(crate) fn patch_previews(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, previews: &HashMap<Uid, String>, mentions: &HashMap<Uid, bool>) {
    let all: Vec<EmailSummary> = message_list
        .all_messages()
        .into_iter()
        .map(|mut m| {
            if m.mailbox == *mailbox {
                if let Some(p) = previews.get(&m.uid) {
                    m.preview = Some(p.clone());
                }
                if let Some(mentions) = mentions.get(&m.uid) {
                    m.mentions = *mentions;
                }
            }
            m
        })
        .collect();
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
}
