//! The stacked conversation view: layout, body loading and the WebView script messages.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::text::human_size;
use lookout_core::{BodyPart, EmailBody, EmailSummary, MailboxId, MailboxRole, Uid, UidValidity};
use lookout_mail::session::AccountCommand;
use webkit::prelude::*;

use crate::message_list::ThreadId;
use crate::worker::Worker;

use super::*;

/// The reading pane's conversation page widgets (item 1.7).
pub(crate) struct ConversationUi {
    pub(crate) subject_label: gtk::Label,
    pub(crate) theme_button: gtk::ToggleButton,
    pub(crate) web_view: webkit::WebView,
    pub(crate) reading_stack: gtk::Stack,
    /// Set while the app itself flips `theme_button`, so its handler doesn't
    /// rebuild the document for a change that isn't the reader's.
    pub(crate) suppress_theme: Cell<bool>,
}

/// The conversation on the reading pane (item 1.7).
pub(crate) struct ConversationPane {
    pub(crate) id: ThreadId,
    /// The listed messages it was built from: a list refresh that changes
    /// them rebuilds the view, one that only changes their flags updates
    /// the cards in place.
    pub(crate) listed: Vec<EmailSummary>,
    pub(crate) cards: Vec<crate::conversation_view::Card>,
    /// The list row the reader selected, or `None` for the header.
    pub(crate) focus: Option<(MailboxId, Uid)>,
    /// Bumped on every build, so a late cache read for an older build is
    /// dropped.
    pub(crate) generation: u64,
    /// The document has finished loading, so script calls can reach it.
    pub(crate) ready: bool,
    /// Script calls made before `ready`, run in order once it is.
    pub(crate) queued: Vec<String>,
    /// A scroll offset to restore once a rebuilt document has loaded.
    pub(crate) restore_scroll: Option<f64>,
    /// Cards whose blocked remote content the reader chose to show.
    pub(crate) load_once: HashSet<(MailboxId, Uid)>,
    /// The "Switch message theme" override, for every card.
    pub(crate) dark: bool,
    /// Opened unread cards waiting on the "after a delay" mark-as-read
    /// timer, and the timer.
    pub(crate) mark_pending: Vec<(MailboxId, Uid)>,
    pub(crate) mark_timer: Option<glib::SourceId>,
}

/// What a conversation script message needs to act.
#[derive(Clone)]
pub(crate) struct ConversationContext {
    pub(crate) state: Rc<RefCell<UiState>>,
    pub(crate) worker: Rc<Worker>,
    pub(crate) message_header: crate::message_header::MessageHeader,
    pub(crate) profile_card: crate::profile_card::ContextProvider,
    pub(crate) menu: MessageMenuContext,
}

pub(crate) fn next_conversation_generation() -> u64 {
    thread_local! {
        static NEXT: Cell<u64> = const { Cell::new(0) };
    }
    NEXT.with(|next| {
        next.set(next.get() + 1);
        next.get()
    })
}

/// Runs `script` in the conversation document's script world, or queues it
/// until the document has loaded.
pub(crate) fn run_conversation_script(state: &Rc<RefCell<UiState>>, script: String) {
    let ui = {
        let mut st = state.borrow_mut();
        let Some(pane) = st.rendered_conversation.as_mut() else { return };
        if !pane.ready {
            pane.queued.push(script);
            return;
        }
        st.conversation_ui.clone()
    };
    let Some(ui) = ui else { return };
    ui.web_view
        .evaluate_javascript(&script, Some(crate::conversation_view::SCRIPT_WORLD), None, None::<&gio::Cancellable>, |result| {
            if let Err(e) = result {
                tracing::debug!("conversation script call failed: {e}");
            }
        });
}

/// Resets the single-message view's per-message state when the pane moves
/// to a conversation, so coming back to that message later renders it
/// afresh rather than looking already shown.
pub(crate) fn clear_single_message_state(state: &Rc<RefCell<UiState>>) {
    flush_mark_read_on_leave(state);
    cancel_pending_mark_read(state);
    {
        let mut st = state.borrow_mut();
        st.pending_body_request = None;
        st.body_fetch_error = None;
        st.body_fetch_generation += 1;
        st.pending_html_reveal = false;
        st.reveal_generation += 1;
        st.pending_header = None;
        st.pending_attachment = None;
        st.pending_raw_message = None;
        st.unsubscribe_info = None;
        st.unsubscribe_dismissed = None;
        st.imip = None;
        st.imip_dismissed = None;
        st.read_receipt_request = None;
        st.read_receipt_dismissed = None;
        st.read_receipt_context = None;
        st.rendered_trust_sender = None;
        st.load_once_images = false;
        st.trust_banner_dismissed = None;
        st.rendered_message = None;
        st.rendered_draft = None;
        st.rendered_snooze = None;
        st.rendered_follow_up = None;
        st.pending_draft_open = None;
    }
    drop_pending_cid(state);
}

/// Leaves the conversation on the reading pane, if there is one: marks the
/// cards read that "on-selection-change" mode was holding (item 1.8), stops
/// a pending delayed mark, and drops the conversation's state.
pub(crate) fn leave_conversation(state: &Rc<RefCell<UiState>>) {
    let (pane, on_leave) = {
        let mut st = state.borrow_mut();
        let pane = st.rendered_conversation.take();
        st.conversation_inline_parts.clear();
        (pane, std::mem::take(&mut st.conversation_mark_on_leave))
    };
    let Some(mut pane) = pane else { return };
    if let Some(timer) = pane.mark_timer.take() {
        timer.remove();
    }
    for (mailbox, uid) in on_leave {
        mark_message_read(state, &mailbox, uid);
    }
    drop_pending_cid(state);
}

/// Shows conversation `id` on the reading pane (item 1.7): `listed` are its
/// messages in the list, `focus` the row the reader selected (`None` for
/// the header). The same conversation already on screen only follows the
/// list - new messages, changed flags, a different focus - instead of
/// starting over.
pub(crate) fn open_conversation(ctx: &ConversationContext, id: ThreadId, listed: Vec<EmailSummary>, focus: Option<(MailboxId, Uid)>) {
    let state = &ctx.state;
    let Some(ui) = state.borrow().conversation_ui.clone() else { return };
    let same = state.borrow().rendered_conversation.as_ref().is_some_and(|pane| pane.id == id);
    if same {
        follow_conversation(ctx, &ui, listed, focus);
        return;
    }
    leave_conversation(state);
    clear_single_message_state(state);
    let dark = state.borrow().settings.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK);
    let subject = crate::message_list::newest_message(&listed)
        .and_then(|m| m.subject)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(no subject)".to_string());
    ui.subject_label.set_label(&subject);
    ui.suppress_theme.set(true);
    ui.theme_button.set_active(dark);
    ui.suppress_theme.set(false);
    state.borrow_mut().rendered_conversation = Some(ConversationPane {
        id,
        listed,
        cards: Vec::new(),
        focus,
        generation: 0,
        ready: false,
        queued: Vec::new(),
        restore_scroll: None,
        load_once: HashSet::new(),
        dark,
        mark_pending: Vec::new(),
        mark_timer: None,
    });
    // Fade the previous content out; the conversation fades in once its
    // document has loaded (the WebView's `Finished`).
    ui.reading_stack.set_visible_child_name("empty");
    build_conversation(ctx, None);
}

/// The same conversation is still selected: rebuild it if its messages
/// changed, otherwise refresh the cards whose state changed and follow a new
/// focus.
pub(crate) fn follow_conversation(ctx: &ConversationContext, ui: &Rc<ConversationUi>, listed: Vec<EmailSummary>, focus: Option<(MailboxId, Uid)>) {
    let state = &ctx.state;
    let key = |m: &EmailSummary| (m.mailbox.clone(), m.uid);
    let (members_changed, focus_changed) = {
        let st = state.borrow();
        let Some(pane) = st.rendered_conversation.as_ref() else { return };
        let old: HashSet<(MailboxId, Uid)> = pane.listed.iter().map(key).collect();
        let new: HashSet<(MailboxId, Uid)> = listed.iter().map(key).collect();
        (old != new, focus.is_some() && focus != pane.focus)
    };
    // The reader picked another message of the conversation while one was
    // open on its own ("Show details"): back to the conversation.
    if focus_changed && ui.reading_stack.visible_child_name().as_deref() == Some("message") {
        clear_single_message_state(state);
        ui.reading_stack.set_visible_child_name("conversation");
    }
    if members_changed {
        let keep: HashSet<(MailboxId, Uid)> = {
            let mut st = state.borrow_mut();
            let Some(pane) = st.rendered_conversation.as_mut() else { return };
            pane.listed = listed;
            if focus.is_some() {
                pane.focus = focus;
            }
            pane.cards.iter().filter(|c| c.expanded).map(|c| c.key()).collect()
        };
        rebuild_conversation_keeping_scroll(ctx, ui, keep);
        return;
    }
    // Same messages: refresh any card whose state changed.
    let mut updates = Vec::new();
    {
        let mut st = state.borrow_mut();
        let Some(pane) = st.rendered_conversation.as_mut() else { return };
        for summary in &listed {
            let Some(index) = pane.cards.iter().position(|c| c.key() == key(summary)) else {
                continue;
            };
            let card = &mut pane.cards[index];
            let old = &card.member.summary;
            if old.flags != summary.flags || old.keywords != summary.keywords || old.importance != summary.importance || old.preview != summary.preview {
                card.member.summary = summary.clone();
                updates.push(crate::conversation_view::js_update_card(
                    index,
                    &crate::conversation_view::card_html(index, card, chrono::Utc::now()),
                ));
            }
        }
        pane.listed = listed;
    }
    for script in updates {
        run_conversation_script(state, script);
    }
    if focus_changed {
        let index = {
            let mut st = state.borrow_mut();
            let Some(pane) = st.rendered_conversation.as_mut() else { return };
            pane.focus = focus.clone();
            focus.as_ref().and_then(|f| pane.cards.iter().position(|c| &c.key() == f))
        };
        if let Some(index) = index {
            expand_conversation_card(ctx, index, true);
            run_conversation_script(state, crate::conversation_view::js_scroll_to(index));
        }
    }
}

/// Lays the conversation on screen out again after a setting it depends on
/// changed (its order, whether Sent Items joins it), keeping the open cards
/// and the scroll position.
pub(crate) fn relayout_conversation(ctx: &ConversationContext) {
    let Some(ui) = ctx.state.borrow().conversation_ui.clone() else { return };
    let keep: HashSet<(MailboxId, Uid)> = {
        let st = ctx.state.borrow();
        let Some(pane) = st.rendered_conversation.as_ref() else { return };
        pane.cards.iter().filter(|c| c.expanded).map(|c| c.key()).collect()
    };
    rebuild_conversation_keeping_scroll(ctx, &ui, keep);
}

/// Rebuilds the conversation document (new messages, the theme toggle),
/// keeping the cards in `keep` open and the reader's scroll position.
pub(crate) fn rebuild_conversation_keeping_scroll(ctx: &ConversationContext, ui: &Rc<ConversationUi>, keep: HashSet<(MailboxId, Uid)>) {
    let ctx = ctx.clone();
    let ready = ctx.state.borrow().rendered_conversation.as_ref().is_some_and(|pane| pane.ready);
    if !ready {
        build_conversation(&ctx, Some(keep));
        return;
    }
    ui.web_view.evaluate_javascript(
        crate::conversation_view::JS_SCROLL_Y,
        Some(crate::conversation_view::SCRIPT_WORLD),
        None,
        None::<&gio::Cancellable>,
        move |result| {
            let scroll = result.ok().map(|value| value.to_double());
            if let Some(pane) = ctx.state.borrow_mut().rendered_conversation.as_mut() {
                pane.restore_scroll = scroll;
            }
            build_conversation(&ctx, Some(keep));
        },
    );
}

/// Builds (or rebuilds) the rendered conversation's document: looks up its
/// Sent Items members when the setting asks for them, then lays out the
/// cards. `keep` carries the open cards over a rebuild.
pub(crate) fn build_conversation(ctx: &ConversationContext, keep: Option<HashSet<(MailboxId, Uid)>>) {
    let state = &ctx.state;
    let generation = next_conversation_generation();
    let (listed, sent) = {
        let mut st = state.borrow_mut();
        let Some(pane) = st.rendered_conversation.as_mut() else { return };
        pane.generation = generation;
        let listed = pane.listed.clone();
        let include_sent = st.settings.get_bool(crate::settings::MAIL_CONVERSATION_INCLUDE_SENT);
        let sent = include_sent
            .then(|| listed.first().and_then(|m| mailbox_account_id(&m.mailbox)))
            .flatten()
            .and_then(|account| {
                let handle = st.accounts.get(&account)?;
                let sent = handle.folders.iter().find(|f| f.role == MailboxRole::Sent)?.id.clone();
                Some((sent, handle.address_cache.clone()?))
            })
            .filter(|(sent, _)| listed.iter().all(|m| &m.mailbox != sent));
        (listed, sent)
    };
    let Some((sent, cache)) = sent else {
        lay_out_conversation(ctx, generation, Vec::new(), keep);
        return;
    };
    let ids = crate::conversation_view::conversation_ids(&listed);
    let reply_rx = spawn_cache_read(&ctx.worker, cache, move |c| c.conversation_members_in(&sent, &ids).unwrap_or_default());
    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        let others = reply_rx.recv().await.unwrap_or_default();
        lay_out_conversation(&ctx, generation, others, keep);
    });
}

/// The attachment tiles for a message: its fetched body's parts once
/// loaded, else the parts its cached structure lists.
pub(crate) fn conversation_attachment_tiles(summary: &EmailSummary, body: Option<&EmailBody>) -> Vec<crate::conversation_view::AttachmentTile> {
    let parts: Vec<&BodyPart> = match body {
        Some(body) => body.parts.iter().filter(|p| p.is_attachment).collect(),
        None => summary.structure.iter().flatten().filter(|p| p.is_attachment).collect(),
    };
    parts
        .into_iter()
        .map(|part| crate::conversation_view::AttachmentTile {
            part_number: part.part_number.clone(),
            name: attachment_display_name(part),
            size_label: human_size(part.size.into()),
        })
        .collect()
}

/// Fills card `index` from its loaded body: the frame's document under the
/// sender's remote-content policy, the blocked-content note, attachments,
/// and the inline parts its `cid:` images resolve against.
pub(crate) fn fill_conversation_card(
    state: &Rc<RefCell<UiState>>,
    index: usize,
    card: &mut crate::conversation_view::Card,
    body: &EmailBody,
    load_once: bool,
    dark: bool,
) -> Vec<BodyPart> {
    let summary = &card.member.summary;
    let trust_sender = mailbox_account_id(&summary.mailbox).zip(summary.from.first().map(|a| a.address.trim().to_lowercase()));
    let (images_allowed, all_allowed) = remote_content_policy(state, trust_sender.as_ref(), load_once);
    let scan = body.html_body.as_deref().map(lookout_core::html_remote_content_scan).unwrap_or_default();
    card.remote_blocked = (scan.has_images && !images_allowed) || (scan.has_other && !all_allowed);
    card.wants_receipt = !lookout_core::parse_disposition_notification_to(&body.headers).is_empty();
    card.attachments = conversation_attachment_tiles(summary, Some(body));
    card.srcdoc = Some(crate::conversation_view::body_srcdoc(body, index, images_allowed, all_allowed, dark));
    body.parts.iter().filter(|p| p.cid.is_some()).cloned().collect()
}

/// Lays out the conversation's cards (its listed messages plus `others`
/// from Sent Items) and loads the document, unless a newer build has
/// started since.
pub(crate) fn lay_out_conversation(ctx: &ConversationContext, generation: u64, others: Vec<EmailSummary>, keep: Option<HashSet<(MailboxId, Uid)>>) {
    use crate::conversation_view as view;
    let state = &ctx.state;
    let Some(ui) = state.borrow().conversation_ui.clone() else { return };
    let (listed, focus, dark, load_once) = {
        let st = state.borrow();
        let Some(pane) = st.rendered_conversation.as_ref().filter(|p| p.generation == generation) else {
            return;
        };
        (pane.listed.clone(), pane.focus.clone(), pane.dark, pane.load_once.clone())
    };
    let order = crate::settings::conversation_order(&state.borrow().settings.get_string(crate::settings::MAIL_CONVERSATION_ORDER));
    let members = view::merge_members(listed, others, "Sent Items");
    let mut expanded = view::initially_expanded(&members, focus.as_ref());
    if let Some(keep) = &keep {
        // A rebuild keeps what the reader had open, and opens only what's
        // new and unread (or newest).
        let known: HashSet<(MailboxId, Uid)> = {
            let st = state.borrow();
            st.rendered_conversation.as_ref().map(|p| p.cards.iter().map(|c| c.key()).collect()).unwrap_or_default()
        };
        expanded.retain(|key| !known.contains(key) || keep.contains(key));
        expanded.extend(keep.iter().cloned());
    }
    let members = view::display_order(members, order);
    let mut cards: Vec<view::Card> = members
        .into_iter()
        .map(|member| {
            let expanded = expanded.contains(&member.key());
            let attachments = conversation_attachment_tiles(&member.summary, None);
            view::Card {
                member,
                expanded,
                srcdoc: None,
                remote_blocked: false,
                wants_receipt: false,
                attachments,
            }
        })
        .collect();
    // Bodies already in memory go straight into the document; the rest are
    // fetched for the open cards and arrive through `conversation_body_arrived`.
    let mut inline_parts = HashMap::new();
    let mut missing = Vec::new();
    for (index, card) in cards.iter_mut().enumerate() {
        let key = card.key();
        let body = state.borrow_mut().body_cache.get(&key.0, &key.1);
        match body {
            Some(body) => {
                let parts = fill_conversation_card(state, index, card, &body, load_once.contains(&key), dark);
                inline_parts.insert(key, parts);
            }
            None if card.expanded => missing.push(key),
            None => {}
        }
    }
    let opened: Vec<(MailboxId, Uid)> = cards.iter().filter(|c| c.expanded && c.member.summary.is_unread()).map(|c| c.key()).collect();
    let html = view::build_document(&cards, chrono::Utc::now(), dark);
    {
        let mut st = state.borrow_mut();
        st.conversation_inline_parts = inline_parts;
        let Some(pane) = st.rendered_conversation.as_mut() else { return };
        pane.cards = cards;
        pane.ready = false;
        pane.queued.clear();
    }
    drop_pending_cid(state);
    ui.web_view.load_html(&html, None);
    request_conversation_bodies(ctx, missing);
    conversation_mark_read(ctx, opened);
}

/// Fetches the bodies of the conversation cards `keys`: from the disk cache
/// at once, and from the server (`FetchBody`, which the session coalesces
/// per folder). Whichever lands first fills the card; a body that never
/// comes shows "couldn't be loaded" after the usual grace period.
pub(crate) fn request_conversation_bodies(ctx: &ConversationContext, keys: Vec<(MailboxId, Uid)>) {
    let state = &ctx.state;
    let mut by_mailbox: HashMap<MailboxId, Vec<Uid>> = HashMap::new();
    for (mailbox, uid) in keys {
        by_mailbox.entry(mailbox).or_default().push(uid);
    }
    let network_available = gio::NetworkMonitor::default().is_network_available();
    for (mailbox, uids) in by_mailbox {
        let target = {
            let st = state.borrow();
            mailbox_account_id(&mailbox).and_then(|account| st.accounts.get(&account)).map(|handle| {
                let uidvalidity = handle.folders.iter().find(|f| f.id == mailbox).map(|f| f.uidvalidity).unwrap_or(UidValidity(0));
                (handle.interactive_cmd_tx.clone(), handle.address_cache.clone(), uidvalidity)
            })
        };
        let Some((cmd_tx, cache, uidvalidity)) = target else {
            for uid in &uids {
                conversation_body_failed(state, &mailbox, *uid);
            }
            continue;
        };
        for uid in &uids {
            let _ = cmd_tx.send_blocking(AccountCommand::FetchBody {
                mailbox: mailbox.clone(),
                uid: *uid,
            });
        }
        if let Some(cache) = cache {
            let reply_rx = spawn_cache_read(&ctx.worker, cache, {
                let mailbox = mailbox.clone();
                let uids = uids.clone();
                move |c| c.load_bodies(&mailbox, &uids, uidvalidity)
            });
            let state = state.clone();
            let mailbox = mailbox.clone();
            glib::spawn_future_local(async move {
                let Ok(Ok(bodies)) = reply_rx.recv().await else { return };
                for (uid, body) in bodies {
                    state.borrow_mut().body_cache.insert(mailbox.clone(), uid, body.clone());
                    conversation_body_arrived(&state, &mailbox, uid, body);
                }
            });
        }
        let state = state.clone();
        let timeout = if network_available { BODY_FETCH_TIMEOUT_MS } else { BODY_FETCH_OFFLINE_TIMEOUT_MS };
        glib::timeout_add_local_once(std::time::Duration::from_millis(timeout), move || {
            for uid in uids {
                conversation_body_failed(&state, &mailbox, uid);
            }
        });
    }
}

/// A body arrived (from the server or the disk cache): fill its card if the
/// conversation on screen is still waiting for it. Called for every
/// `BodyFetched`, so it is a cheap no-op for anything else.
pub(crate) fn conversation_body_arrived(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid, body: EmailBody) {
    let key = (mailbox.clone(), uid);
    let (index, mut card, load_once, dark) = {
        let st = state.borrow();
        let Some(pane) = st.rendered_conversation.as_ref() else { return };
        let Some(index) = pane.cards.iter().position(|c| c.key() == key && c.srcdoc.is_none()) else {
            return;
        };
        (index, pane.cards[index].clone(), pane.load_once.contains(&key), pane.dark)
    };
    let parts = fill_conversation_card(state, index, &mut card, &body, load_once, dark);
    let script = crate::conversation_view::js_set_body(&card, index);
    {
        let mut st = state.borrow_mut();
        st.conversation_inline_parts.insert(key.clone(), parts);
        let Some(pane) = st.rendered_conversation.as_mut() else { return };
        match pane.cards.get_mut(index) {
            Some(slot) if slot.key() == key => *slot = card,
            _ => return,
        }
    }
    run_conversation_script(state, script);
}

/// A card's body didn't arrive in time.
pub(crate) fn conversation_body_failed(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid) {
    let index = {
        let st = state.borrow();
        st.rendered_conversation
            .as_ref()
            .and_then(|pane| pane.cards.iter().position(|c| c.key() == (mailbox.clone(), uid) && c.srcdoc.is_none()))
    };
    if let Some(index) = index {
        run_conversation_script(state, crate::conversation_view::js_body_failed(index));
    }
}

/// Opens card `index` (the reader's click, or the list selecting it):
/// fetches its body if need be and counts it as read per the mark-as-read
/// setting. `tell_script` also opens it in the document, for an opening the
/// script didn't start.
pub(crate) fn expand_conversation_card(ctx: &ConversationContext, index: usize, tell_script: bool) {
    let state = &ctx.state;
    let (key, unread, has_body, was_expanded) = {
        let mut st = state.borrow_mut();
        let Some(card) = st.rendered_conversation.as_mut().and_then(|pane| pane.cards.get_mut(index)) else {
            return;
        };
        let was_expanded = card.expanded;
        card.expanded = true;
        (card.key(), card.member.summary.is_unread(), card.srcdoc.is_some(), was_expanded)
    };
    if tell_script && !was_expanded {
        run_conversation_script(state, crate::conversation_view::js_set_expanded(index, true));
    }
    if !has_body {
        let cached = state.borrow_mut().body_cache.get(&key.0, &key.1);
        match cached {
            Some(body) => conversation_body_arrived(state, &key.0, key.1, body),
            None => request_conversation_bodies(ctx, vec![key.clone()]),
        }
    }
    if unread && !was_expanded {
        conversation_mark_read(ctx, vec![key]);
    }
}

/// Counts the opened unread cards `keys` as read, per the mark-as-read
/// setting (item 1.8) - when opened, after the delay (if the conversation is
/// still on screen), or once the reader leaves the conversation.
pub(crate) fn conversation_mark_read(ctx: &ConversationContext, keys: Vec<(MailboxId, Uid)>) {
    if keys.is_empty() {
        return;
    }
    let state = &ctx.state;
    let mode = crate::settings::mark_as_read_mode(&state.borrow().settings.get_string(crate::settings::MARK_AS_READ_MODE));
    match mode {
        crate::settings::MarkAsReadMode::WhenOpened => {
            for (mailbox, uid) in keys {
                mark_message_read(state, &mailbox, uid);
            }
        }
        crate::settings::MarkAsReadMode::OnSelectionChange => {
            let mut st = state.borrow_mut();
            for key in keys {
                if !st.conversation_mark_on_leave.contains(&key) {
                    st.conversation_mark_on_leave.push(key);
                }
            }
        }
        crate::settings::MarkAsReadMode::AfterDelay => {
            let delay = crate::settings::mark_as_read_delay(state.borrow().settings.get_int(crate::settings::MARK_AS_READ_DELAY));
            let id = {
                let mut st = state.borrow_mut();
                let Some(pane) = st.rendered_conversation.as_mut() else { return };
                pane.mark_pending.extend(keys);
                if let Some(timer) = pane.mark_timer.take() {
                    timer.remove();
                }
                pane.id.clone()
            };
            let state_for_timer = state.clone();
            let timer = glib::timeout_add_local_once(std::time::Duration::from_secs(u64::from(delay.max(1) as u32)), move || {
                let pending = {
                    let mut st = state_for_timer.borrow_mut();
                    let Some(pane) = st.rendered_conversation.as_mut().filter(|pane| pane.id == id) else {
                        return;
                    };
                    pane.mark_timer = None;
                    std::mem::take(&mut pane.mark_pending)
                };
                for (mailbox, uid) in pending {
                    mark_message_read(&state_for_timer, &mailbox, uid);
                }
            });
            if let Some(pane) = state.borrow_mut().rendered_conversation.as_mut() {
                pane.mark_timer = Some(timer);
            }
        }
        crate::settings::MarkAsReadMode::Never => {}
    }
}

/// The message on conversation card `index`.
pub(crate) fn conversation_card_summary(state: &Rc<RefCell<UiState>>, index: usize) -> Option<EmailSummary> {
    let st = state.borrow();
    st.rendered_conversation.as_ref()?.cards.get(index).map(|card| card.member.summary.clone())
}

/// Acts on a message posted by the conversation script.
pub(crate) fn handle_conversation_message(ctx: &ConversationContext, ui: &Rc<ConversationUi>, message: crate::conversation_view::ScriptMessage) {
    use crate::conversation_view::ScriptMessage;
    let state = &ctx.state;
    match message {
        ScriptMessage::Expand(index) => expand_conversation_card(ctx, index, false),
        ScriptMessage::Collapse(index) => {
            if let Some(card) = state.borrow_mut().rendered_conversation.as_mut().and_then(|pane| pane.cards.get_mut(index)) {
                card.expanded = false;
            }
        }
        ScriptMessage::Reply(index) | ScriptMessage::ReplyAll(index) | ScriptMessage::Forward(index) => {
            let Some(summary) = conversation_card_summary(state, index) else { return };
            let mode = match message {
                ScriptMessage::Reply(_) => Some(crate::compose::ReplyMode::Reply),
                ScriptMessage::ReplyAll(_) => Some(crate::compose::ReplyMode::ReplyAll),
                _ => None,
            };
            reply_to_message(state, &ctx.worker, &ui.reading_stack, summary, mode);
        }
        ScriptMessage::Menu { card, x, y } => {
            let Some(summary) = conversation_card_summary(state, card) else { return };
            open_message_menu(&ctx.menu, ui.web_view.upcast_ref(), Some((x, y)), MessageMenuTarget::Message(Box::new(summary)));
        }
        ScriptMessage::Attachment { card, part, x, y } => {
            let Some(summary) = conversation_card_summary(state, card) else { return };
            let body = state.borrow_mut().body_cache.get(&summary.mailbox, &summary.uid);
            let parts: Vec<BodyPart> = match &body {
                Some(body) => body.parts.clone(),
                None => summary.structure.clone().unwrap_or_default(),
            };
            let Some(part) = parts.into_iter().find(|p| p.part_number == part) else { return };
            present_conversation_attachment_menu(state, ui, &summary, part, x, y);
        }
        ScriptMessage::ShowBlocked(index) => {
            let Some(summary) = conversation_card_summary(state, index) else { return };
            let key = (summary.mailbox.clone(), summary.uid);
            let body = state.borrow_mut().body_cache.get(&key.0, &key.1);
            let Some(body) = body else { return };
            let (mut card, dark) = {
                let mut st = state.borrow_mut();
                let Some(pane) = st.rendered_conversation.as_mut() else { return };
                pane.load_once.insert(key.clone());
                let Some(card) = pane.cards.get(index) else { return };
                (card.clone(), pane.dark)
            };
            fill_conversation_card(state, index, &mut card, &body, true, dark);
            let script = crate::conversation_view::js_set_body(&card, index);
            if let Some(slot) = state.borrow_mut().rendered_conversation.as_mut().and_then(|pane| pane.cards.get_mut(index)) {
                *slot = card;
            }
            run_conversation_script(state, script);
        }
        ScriptMessage::OpenDetails(index) => {
            let Some(summary) = conversation_card_summary(state, index) else { return };
            show_conversation_message_alone(ctx, ui, summary);
        }
        ScriptMessage::Sender { card, x, y } => {
            let Some(summary) = conversation_card_summary(state, card) else { return };
            let Some(sender) = summary.from.first().cloned() else { return };
            crate::profile_card::show_person_card_at(
                ctx.profile_card.clone(),
                ui.web_view.upcast_ref::<gtk::Widget>(),
                &sender,
                mailbox_account_id(&summary.mailbox),
                x,
                y,
            );
        }
    }
}

/// "Show details" on a card: the message on the single-message page, with
/// every banner it has (invitation, read receipt, unsubscribe) and a way
/// back to the conversation.
pub(crate) fn show_conversation_message_alone(ctx: &ConversationContext, ui: &Rc<ConversationUi>, summary: EmailSummary) {
    let state = &ctx.state;
    clear_single_message_state(state);
    let (mailbox, uid) = (summary.mailbox.clone(), summary.uid);
    state.borrow_mut().pending_header = Some(summary);
    let cached = state.borrow_mut().body_cache.get(&mailbox, &uid);
    match cached {
        Some(body) => {
            ui.reading_stack.set_visible_child_name("empty");
            render_body(state, &ui.reading_stack, &ctx.message_header, mailbox, uid, body);
        }
        None => request_body_fetch(state, &ctx.worker, &ui.reading_stack, &ctx.message_header, mailbox, uid),
    }
}

/// The Open / Open With… / Save… menu for an attachment tile on a card,
/// pointing at the tile.
pub(crate) fn present_conversation_attachment_menu(state: &Rc<RefCell<UiState>>, ui: &Rc<ConversationUi>, summary: &EmailSummary, part: BodyPart, x: f64, y: f64) {
    // The fetch disables a button while it runs; a tile has none, so it
    // gets a stand-in that nothing shows.
    let stand_in = gtk::MenuButton::new();
    let popover = gtk::Popover::new();
    popover.set_parent(&ui.web_view);
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.set_position(gtk::PositionType::Bottom);
    let items = gtk::Box::new(gtk::Orientation::Vertical, 0);
    items.set_margin_top(6);
    items.set_margin_bottom(6);
    items.set_margin_start(6);
    items.set_margin_end(6);
    for (label, action) in [
        ("Open", PendingAttachmentAction::Open),
        ("Open With…", PendingAttachmentAction::OpenWith),
        ("Save…", PendingAttachmentAction::Save),
    ] {
        let item = gtk::Button::with_label(label);
        item.add_css_class("flat");
        item.set_halign(gtk::Align::Start);
        let state = state.clone();
        let mailbox = summary.mailbox.clone();
        let uid = summary.uid;
        let part = part.clone();
        let stand_in = stand_in.clone();
        let popover_for_item = popover.clone();
        item.connect_clicked(move |_| {
            popover_for_item.popdown();
            start_attachment_fetch(&state, &mailbox, uid, &part, &stand_in, action);
        });
        items.append(&item);
    }
    // "Save all" / "Download as zip" (item 1.23 / M-33), when the card has
    // more than one attachment. The dialog anchors to the reading pane's
    // WebView, which the popover is parented to.
    let batch_parts: Vec<BodyPart> = summary.structure.iter().flatten().filter(|p| p.is_attachment).cloned().collect();
    if batch_parts.len() >= 2 {
        for (label, zip) in [("Save all…", false), ("Download as zip…", true)] {
            let item = gtk::Button::with_label(label);
            item.add_css_class("flat");
            item.set_halign(gtk::Align::Start);
            let state = state.clone();
            let mailbox = summary.mailbox.clone();
            let uid = summary.uid;
            let parts = batch_parts.clone();
            let popover_for_item = popover.clone();
            let anchor = ui.web_view.clone();
            item.connect_clicked(move |_| {
                popover_for_item.popdown();
                let state = state.clone();
                let mailbox = mailbox.clone();
                let parts = parts.clone();
                let anchor = anchor.clone();
                glib::spawn_future_local(async move {
                    let Some(window) = anchor.root().and_downcast::<gtk::Window>() else { return };
                    if zip {
                        let dialog = gtk::FileDialog::builder().title("Download attachments").initial_name("attachments.zip").build();
                        let Ok(file) = dialog.save_future(Some(&window)).await else { return };
                        start_attachment_batch(&state, &mailbox, uid, parts, AttachmentBatchAction::Zip(file));
                    } else {
                        let dialog = gtk::FileDialog::builder().title("Save all attachments").build();
                        let Ok(folder) = dialog.select_folder_future(Some(&window)).await else { return };
                        start_attachment_batch(&state, &mailbox, uid, parts, AttachmentBatchAction::SaveAll(folder));
                    }
                });
            });
            items.append(&item);
        }
    }
    popover.set_child(Some(&items));
    popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
    });
    popover.popup();
}

/// Builds the reading pane's conversation page (item 1.7): the subject and
/// the message-theme toggle over the conversation WebView, with its script,
/// message handler and navigation policy.
pub(crate) fn build_conversation_ui(ctx: &ConversationContext, reading_stack: &gtk::Stack) -> (Rc<ConversationUi>, gtk::Box) {
    use crate::conversation_view as view;
    let settings = webkit::Settings::new();
    // The trusted script needs JavaScript on; markup JavaScript stays off,
    // so nothing in the page itself - `<script>`, `on…=` handlers,
    // `javascript:` links - ever runs. Hardware acceleration is left at the
    // default, as on the single-message view: this view is long-lived,
    // reloaded and scrolled in place (see that view's comment).
    settings.set_enable_javascript(true);
    settings.set_enable_javascript_markup(false);
    settings.set_enable_developer_extras(false);
    let user_content_manager = webkit::UserContentManager::new();
    user_content_manager.add_script(&webkit::UserScript::for_world(
        view::SCRIPT,
        webkit::UserContentInjectedFrames::AllFrames,
        webkit::UserScriptInjectionTime::End,
        view::SCRIPT_WORLD,
        &[],
        &[],
    ));
    user_content_manager.register_script_message_handler(view::SCRIPT_HANDLER, Some(view::SCRIPT_WORLD));
    let web_view = webkit::WebView::builder()
        .settings(&settings)
        .user_content_manager(&user_content_manager)
        .hexpand(true)
        .vexpand(true)
        .build();
    // The conversation's message cards are HTML; "All text" blur covers the
    // whole view.
    crate::privacy::mark_blur_all(&web_view);
    web_view.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    install_web_view_zoom(&web_view);

    let subject_label = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["title-3"])
        .build();
    subject_label.set_selectable(true);
    let theme_button = gtk::ToggleButton::builder()
        .icon_name(themed_icon_name(&["weather-clear-night-symbolic", "night-light-symbolic", "weather-clear-symbolic"]))
        .tooltip_text("View in Dark Mode")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(16)
        .margin_end(12)
        .margin_top(10)
        .margin_bottom(4)
        .build();
    header.append(&subject_label);
    header.append(&theme_button);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&header);
    page.append(&web_view);

    let ui = Rc::new(ConversationUi {
        subject_label,
        theme_button: theme_button.clone(),
        web_view: web_view.clone(),
        reading_stack: reading_stack.clone(),
        suppress_theme: Cell::new(false),
    });

    {
        let ctx = ctx.clone();
        let ui = ui.clone();
        user_content_manager.connect_script_message_received(Some(view::SCRIPT_HANDLER), move |_, value| match view::parse_script_message(&value.to_str()) {
            Some(message) => handle_conversation_message(&ctx, &ui, message),
            None => tracing::debug!("ignoring malformed conversation script message"),
        });
    }
    // The document loaded: run what was queued for it, restore the scroll
    // a rebuild saved (or bring the selected card into view), and fade the
    // page in if it isn't showing yet.
    {
        let state = ctx.state.clone();
        let ui_for_load = ui.clone();
        web_view.connect_load_changed(move |_, event| {
            if event != webkit::LoadEvent::Finished {
                return;
            }
            let (queued, restore, focus) = {
                let mut st = state.borrow_mut();
                let Some(pane) = st.rendered_conversation.as_mut() else { return };
                if pane.ready {
                    return;
                }
                pane.ready = true;
                let focus = pane.focus.as_ref().and_then(|f| pane.cards.iter().position(|c| &c.key() == f));
                (std::mem::take(&mut pane.queued), pane.restore_scroll.take(), focus)
            };
            if matches!(ui_for_load.reading_stack.visible_child_name().as_deref(), Some("empty") | Some("loading")) {
                ui_for_load.reading_stack.set_visible_child_name("conversation");
            }
            for script in queued {
                run_conversation_script(&state, script);
            }
            match (restore, focus) {
                (Some(y), _) => run_conversation_script(&state, view::js_restore_scroll(y)),
                (None, Some(index)) => run_conversation_script(&state, view::js_scroll_to(index)),
                (None, None) => {}
            }
        });
    }
    {
        let ctx = ctx.clone();
        let ui_for_theme = ui.clone();
        theme_button.connect_toggled(move |button| {
            // A moon while off, a sun while on, as on the single-message
            // header; the tooltip names what a click does next.
            button.set_icon_name(if button.is_active() {
                themed_icon_name(&["weather-clear-symbolic", "display-brightness-symbolic"])
            } else {
                themed_icon_name(&["weather-clear-night-symbolic", "night-light-symbolic", "weather-clear-symbolic"])
            });
            button.set_tooltip_text(Some(if button.is_active() { "View in Light Mode" } else { "View in Dark Mode" }));
            if ui_for_theme.suppress_theme.get() {
                return;
            }
            let keep = {
                let mut st = ctx.state.borrow_mut();
                let Some(pane) = st.rendered_conversation.as_mut() else { return };
                pane.dark = button.is_active();
                pane.cards.iter().filter(|c| c.expanded).map(|c| c.key()).collect()
            };
            rebuild_conversation_keeping_scroll(&ctx, &ui_for_theme, keep);
        });
    }
    // Navigation: the document and its body frames load in place (they're
    // `about:` documents); a link - retargeted to a new window by the script
    // - goes to the browser through `create`; nothing else may load. Remote
    // subresources are held by each frame's CSP.
    web_view.connect_decide_policy(|_view, decision, decision_type| {
        if decision_type != webkit::PolicyDecisionType::NavigationAction {
            return false;
        }
        let Some(navigation) = decision.downcast_ref::<webkit::NavigationPolicyDecision>().and_then(|d| d.navigation_action()) else {
            return false;
        };
        let Some(uri) = navigation.request().and_then(|r| r.uri()) else { return false };
        let scheme = uri.split(':').next().unwrap_or("");
        if scheme == "about" {
            return false;
        }
        if navigation.navigation_type() == webkit::NavigationType::LinkClicked || navigation.is_user_gesture() {
            open_uri_in_default_browser(&uri);
        }
        decision.ignore();
        true
    });
    web_view.connect_create(|_view, navigation_action| {
        let is_link_click = navigation_action.navigation_type() == webkit::NavigationType::LinkClicked;
        if is_link_click || navigation_action.is_user_gesture() {
            if let Some(uri) = navigation_action.request().and_then(|r| r.uri()) {
                open_uri_in_default_browser(&uri);
            }
        }
        None
    });
    (ui, page)
}
