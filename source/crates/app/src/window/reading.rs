//! The reading pane pipeline: body fetch and render, CID requests, details/source dialogs, print, read receipts, CSP and zoom.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::{AccountId, EmailAddress, EmailSummary, MailboxId, Uid, UidValidity};
use lookout_mail::session::AccountCommand;
use webkit::prelude::*;

use crate::worker::Worker;

use super::*;

pub(crate) fn body_request_matches(mailbox: &MailboxId, uid: &Uid, pending_request: Option<&(MailboxId, Uid)>) -> bool {
    pending_request.is_some_and(|(pending_mailbox, pending_uid)| pending_mailbox == mailbox && pending_uid == uid)
}

/// The reading pane's "message unavailable" text. `network_available`
/// distinguishes a down connection from an unreachable server, so the message
/// points the user at the right thing to check.
pub(crate) fn body_fetch_error_description(network_available: bool) -> &'static str {
    if network_available {
        "The message couldn't be loaded. The server may be unavailable — try again in a moment."
    } else {
        "The message couldn't be loaded. You appear to be offline — check your network connection."
    }
}

/// Stops the reading pane's body-loading spinner, if it's mounted.
pub(crate) fn stop_body_loading(reading_stack: &gtk::Stack) {
    if let Some(spinner) = find_named_descendant(reading_stack, "body-loading-spinner").and_then(|c| c.downcast::<gtk::Spinner>().ok()) {
        spinner.stop();
    }
}

/// Puts the reading pane on its loading page and starts the spinner.
pub(crate) fn show_body_fetch_loading(reading_stack: &gtk::Stack) {
    if let Some(spinner) = find_named_descendant(reading_stack, "body-loading-spinner").and_then(|c| c.downcast::<gtk::Spinner>().ok()) {
        spinner.start();
    }
    reading_stack.set_visible_child_name("loading");
}

/// Clears every trace of an in-flight or failed body fetch - the pending
/// request, the error marker, the loading spinner - and bumps
/// `body_fetch_generation` so a timeout armed for the abandoned fetch can't
/// fire. Called once a body actually renders (see `render_body`).
pub(crate) fn clear_body_fetch_state(state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack) {
    {
        let mut st = state.borrow_mut();
        st.pending_body_request = None;
        st.body_fetch_error = None;
        st.body_fetch_generation += 1;
    }
    stop_body_loading(reading_stack);
}

/// Shows the reading pane's "message unavailable" page for a body fetch that
/// failed while it was still the message on screen. No-op once the user has
/// moved on (the pending request no longer matches).
pub(crate) fn show_body_fetch_error(state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, mailbox: MailboxId, uid: Uid) {
    {
        let mut st = state.borrow_mut();
        if st.pending_body_request.as_ref() != Some(&(mailbox.clone(), uid)) {
            return;
        }
        st.pending_body_request = None;
        st.body_fetch_error = Some((mailbox, uid));
        st.body_fetch_generation += 1;
    }
    stop_body_loading(reading_stack);
    let network_available = gio::NetworkMonitor::default().is_network_available();
    if let Some(page) = reading_stack.child_by_name("error").and_then(|c| c.downcast::<adw::StatusPage>().ok()) {
        page.set_description(Some(body_fetch_error_description(network_available)));
    }
    reading_stack.set_visible_child_name("error");
}

/// Arms the fetch-failure backstop. `generation` is the value captured when
/// the fetch was dispatched; the callback only raises the error page while the
/// request is still pending under that same generation, so a fetch that
/// rendered, was superseded, or was abandoned can't be reported as failed.
pub(crate) fn arm_body_fetch_timeout(state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, generation: u64, delay_ms: u64) {
    let state_for_timeout = state.clone();
    let reading_stack_for_timeout = reading_stack.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(delay_ms), move || {
        let request = {
            let st = state_for_timeout.borrow();
            if st.body_fetch_generation != generation {
                return;
            }
            st.pending_body_request.clone()
        };
        let Some((mailbox, uid)) = request else { return };
        tracing::debug!(?mailbox, uid = uid.0, "FetchBody: timed out; showing unavailable page");
        show_body_fetch_error(&state_for_timeout, &reading_stack_for_timeout, mailbox, uid);
    });
}

/// Dispatches an `AccountCommand::FetchBody` for `(mailbox, uid)` and wires the
/// reading pane's loading/error lifecycle around it: the pending-request
/// marker, the failure backstop, the loading page, and the parallel disk-cache
/// read that can render without waiting for the session. Shared by the
/// message-selection handler, the error page's Retry button, and the automatic
/// retry that follows a reconnect.
pub(crate) fn request_body_fetch(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
    message_header: &crate::message_header::MessageHeader,
    mailbox: MailboxId,
    uid: Uid,
) {
    let (cmd_tx, cache, uidvalidity) = {
        let st = state.borrow();
        let Some(account_id) = mailbox_account_id(&mailbox) else { return };
        let Some(handle) = st.accounts.get(&account_id) else { return };
        let uidvalidity = handle.folders.iter().find(|f| f.id == mailbox).map(|f| f.uidvalidity).unwrap_or(UidValidity(0));
        (handle.interactive_cmd_tx.clone(), handle.address_cache.clone(), uidvalidity)
    };
    // A fresh request supersedes any earlier failure and arms the backstop
    // under a new generation. When the network is already known to be down,
    // use a short grace period: a local disk-cache hit normally answers in
    // milliseconds, so the pane can report the outage quickly instead of
    // spinning for the full timeout.
    let network_available = gio::NetworkMonitor::default().is_network_available();
    let generation = {
        let mut st = state.borrow_mut();
        st.body_fetch_error = None;
        st.pending_body_request = Some((mailbox.clone(), uid));
        st.body_fetch_generation += 1;
        st.body_fetch_generation
    };
    tracing::debug!(?mailbox, uid = uid.0, "FetchBody: dispatching to account actor");
    let _ = cmd_tx.send_blocking(AccountCommand::FetchBody { mailbox: mailbox.clone(), uid });
    show_body_fetch_loading(reading_stack);
    arm_body_fetch_timeout(
        state,
        reading_stack,
        generation,
        if network_available { BODY_FETCH_TIMEOUT_MS } else { BODY_FETCH_OFFLINE_TIMEOUT_MS },
    );
    // Parallel disk fast-path: a body cached on disk but not in the 25-entry
    // in-memory LRU renders the instant this read lands, without waiting for
    // the command to round-trip through the session (the session's own
    // `FetchBody` still serves the same row from its own cache check and
    // answers without a network round trip; whichever lands first renders and
    // the other is deduped by `pending_body_request`).
    let Some(cache) = cache else { return };
    let reply_rx = spawn_cache_read(worker, cache, {
        let mailbox = mailbox.clone();
        move |c| c.load_bodies(&mailbox, &[uid], uidvalidity)
    });
    let state = state.clone();
    let reading_stack = reading_stack.clone();
    let message_header = message_header.clone();
    let request_key = (mailbox.clone(), uid);
    let mailbox_for_render = mailbox;
    glib::spawn_future_local(async move {
        let Ok(result) = reply_rx.recv().await else { return };
        let Ok(bodies) = result else { return };
        let Some(body) = bodies.get(&uid).cloned() else { return };
        let still_current = state.borrow().pending_body_request.as_ref() == Some(&request_key);
        if !still_current {
            return;
        }
        state.borrow_mut().pending_body_request = None;
        tracing::debug!(?mailbox_for_render, uid = uid.0, "FetchBody: serving from disk cache");
        render_body(&state, &reading_stack, &message_header, mailbox_for_render, uid, body);
    });
}

/// Finishes a WebKit URI-scheme request with an error, so the browser renders
/// a broken image instead of hanging the page's load on the subresource.
pub(crate) fn finish_cid_request_error(request: &webkit::URISchemeRequest, message: &str) {
    request.finish_error(&mut glib::Error::new(gio::IOErrorEnum::Failed, message));
}

/// Finishes every in-flight inline `cid:` request with an error - called
/// whenever the reading pane moves off the message those requests belong to
/// (new selection, new render, account teardown), so WebKit never waits on
/// subresources of a message the user has left.
pub(crate) fn drop_pending_cid(state: &Rc<RefCell<UiState>>) {
    let pending = std::mem::take(&mut state.borrow_mut().pending_cid);
    for ((_, _, part_number), requests) in pending {
        tracing::debug!(part = %part_number, "cid: dropping pending inline-image request");
        for request in requests {
            finish_cid_request_error(&request, "the message changed before the inline image arrived");
        }
    }
}

/// Resolves a `cid:` reference requested by the reading pane's WebKit view
/// to the matching inline part of the currently-rendered message, and asks
/// that account's session for the part's bytes (`FetchAttachment`). Runs on
/// the main loop (the scheme handler itself only forwards the request here,
/// because it fires on a WebKit worker thread). A reference that matches no
/// part of the current message is answered with an error immediately, so an
/// unknown id can't wedge the pane; so is a reference arriving while no
/// message is on screen. Each dispatched request is tracked in
/// `UiState::pending_cid` until `PartFetched`/`PartFetchFailed` (or the
/// timeout) lands.
pub(crate) fn dispatch_cid_request(state: &Rc<RefCell<UiState>>, cid: &str, request: webkit::URISchemeRequest) {
    let target = (|| {
        let st = state.borrow();
        // A conversation card's reference names its card (item 1.7); any
        // other is the single-message view's.
        let (mailbox, uid, cid, parts) = match crate::conversation_view::split_card_cid(cid) {
            Some((card, cid)) => {
                let pane = st.rendered_conversation.as_ref()?;
                let (mailbox, uid) = pane.cards.get(card)?.key();
                let parts = st.conversation_inline_parts.get(&(mailbox.clone(), uid))?.clone();
                (mailbox, uid, cid, parts)
            }
            None => {
                let (mailbox, uid) = st.rendered_message.clone()?;
                (mailbox, uid, cid, st.rendered_inline_parts.clone())
            }
        };
        let part = parts.iter().find(|p| p.cid.as_deref().is_some_and(|c| lookout_core::cid_matches(cid, c)))?.clone();
        let cmd_tx = mailbox_account_id(&mailbox).and_then(|id| st.accounts.get(&id)).map(|h| h.interactive_cmd_tx.clone())?;
        Some((mailbox, uid, part, cmd_tx))
    })();
    let Some((mailbox, uid, part, cmd_tx)) = target else {
        finish_cid_request_error(&request, "this message has no matching inline image");
        return;
    };
    let key = (mailbox.clone(), uid, part.part_number.clone());
    let already_fetching = {
        let mut st = state.borrow_mut();
        let waiting = st.pending_cid.entry(key.clone()).or_default();
        waiting.push(request);
        waiting.len() > 1
    };
    if already_fetching {
        return;
    }
    tracing::debug!(?mailbox, uid = uid.0, cid, part = %part.part_number, "cid: inline image request dispatched to account actor");
    let _ = cmd_tx.send_blocking(AccountCommand::FetchAttachment { mailbox, uid, part });
    arm_cid_timeout(state, key);
}

/// Backstop for a `cid:` image fetch whose answer is lost (e.g. the session
/// dies mid-fetch and the command disappears into the reconnect): finish the
/// WebKit request with an error after a generous grace period instead of
/// letting the page hang on it. Only fires if this exact request is still
/// the outstanding one.
pub(crate) fn arm_cid_timeout(state: &Rc<RefCell<UiState>>, key: PendingCidKey) {
    let state_for_timeout = state.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(ATTACHMENT_FETCH_TIMEOUT_MS), move || {
        let Some(requests) = state_for_timeout.borrow_mut().pending_cid.remove(&key) else {
            return glib::ControlFlow::Break;
        };
        tracing::warn!(part = %key.2, "cid: inline image fetch timed out");
        for request in requests {
            finish_cid_request_error(&request, "the inline image could not be fetched");
        }
        glib::ControlFlow::Break
    });
}

/// Reveals the reading pane's "message" page (switching `content_stack` to
/// `content_page` and the reading stack to "message"). If the stack is
/// mid-transition - e.g. a message → "empty" fade-out still settling after a
/// fast body load - the reveal is deferred until that transition completes,
/// so the next message never appears before the previous one has fully faded
/// away.
pub(crate) fn reveal_message_page(reading_stack: &gtk::Stack, content_stack: &gtk::Stack, content_page: &'static str) {
    if reading_stack.is_transition_running() {
        let reading_stack = reading_stack.clone();
        let content_stack = content_stack.clone();
        let settled = Rc::new(Cell::new(None::<glib::SignalHandlerId>));
        let settled_for_notify = settled.clone();
        let handler_id = reading_stack.connect_notify_local(Some("transition-running"), move |stack, _| {
            if !stack.is_transition_running() {
                if let Some(handler_id) = settled_for_notify.take() {
                    stack.disconnect(handler_id);
                }
                content_stack.set_visible_child_name(content_page);
                stack.set_visible_child_name("message");
            }
        });
        settled.set(Some(handler_id));
    } else {
        content_stack.set_visible_child_name(content_page);
        reading_stack.set_visible_child_name("message");
    }
}

/// Locates a named child of the reading pane's `"message"` page by walking
/// its children - used to find widgets by `widget_name()` rather than relying
/// on their position among siblings, since the page mixes fixed structural
/// children (attachment strip, body stack) with ones the header/action-bar
/// widgets add around them.
pub(crate) fn find_named_child(reading_stack: &gtk::Stack, name: &str) -> Option<gtk::Widget> {
    let page = reading_stack.child_by_name("message").and_downcast::<gtk::Box>()?;
    let mut child = page.first_child();
    while let Some(c) = child {
        if c.widget_name() == name {
            return Some(c);
        }
        child = c.next_sibling();
    }
    None
}

/// Walks the widget tree under `root` depth-first looking for the first widget
/// whose `widget_name()` is `name`. Used to reach into the reading stack's
/// less-structured pages (a spinner nested in a box, a `StatusPage` with its
/// own internal tree) without depending on their sibling order.
pub(crate) fn find_named_descendant(root: &impl IsA<gtk::Widget>, name: &str) -> Option<gtk::Widget> {
    let root: &gtk::Widget = root.upcast_ref();
    if root.widget_name() == name {
        return Some(root.clone());
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(found) = find_named_descendant(&c, name) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

/// Locates the reading pane's attachment strip - the named `gtk::Box`
/// (`"attachments"`) that `build_window` inserted between the message header
/// and the body `content_stack`.
pub(crate) fn find_attachment_strip(reading_stack: &gtk::Stack) -> Option<gtk::Box> {
    find_named_child(reading_stack, "attachments")?.downcast::<gtk::Box>().ok()
}

/// Returns the first direct child of `root` whose `widget_name()` is `name`.
/// The invite card's fixed row structure is looked up this way (rather than
/// holding widget references across `render_body` calls) so `render_invite_card`
/// can repopulate the card the same way `rebuild_attachment_strip` finds the
/// strip.
pub(crate) fn named_child_of(root: &gtk::Box, name: &str) -> Option<gtk::Widget> {
    let mut child = root.first_child();
    while let Some(c) = child {
        if c.widget_name() == name {
            return Some(c);
        }
        child = c.next_sibling();
    }
    None
}

/// Opens Outlook's "View message details" dialog (item 1.10 / M-30): the
/// message's key fields, its full parsed internet-header block, and the raw
/// RFC 5322 source on a Source tab. The key fields come from the summary so
/// they render immediately; the header block renders from the body cache
/// when the message has already been read this session, and both the header
/// block and the source fill in when `FetchRawMessage` answers
/// (`deliver_raw_to_details_dialog`). One dialog at a time - opening it for
/// the same message re-presents the existing one, and for a different
/// message replaces it.
pub(crate) fn show_message_details_dialog(state: &Rc<RefCell<UiState>>, parent: &adw::ApplicationWindow, summary: &EmailSummary, page: DetailsPage) {
    // Re-present an existing dialog opened for the same message.
    {
        let st = state.borrow();
        if let Some(existing) = st.message_details.as_ref() {
            if existing.mailbox == summary.mailbox && existing.uid == summary.uid {
                existing.pages.set_visible_child_name(page.name());
                existing.window.present();
                return;
            }
        }
    }
    // A dialog open for a different message is replaced: take it out of the
    // slot first (so its close handler sees nothing to clear), then close it.
    let previous = state.borrow_mut().message_details.take();
    if let Some(previous) = previous {
        previous.window.close();
    }

    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Message details")
        .default_width(780)
        .default_height(640)
        .build();
    let header = adw::HeaderBar::new();
    let close_button = gtk::Button::from_icon_name("window-close-symbolic");
    close_button.set_css_classes(&["flat"]);
    close_button.set_tooltip_text(Some("Close"));
    header.pack_end(&close_button);

    let switcher = adw::ViewSwitcher::builder().policy(adw::ViewSwitcherPolicy::Wide).build();
    header.set_title_widget(Some(&switcher));

    let stack = adw::ViewStack::new();

    // --- Details page: key fields, then the full parsed header block. ---
    let details_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    let fields = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    for (caption, value) in message_detail_fields(summary) {
        fields.append(&message_detail_row(&caption, &value));
    }
    details_content.append(&fields);
    details_content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    details_content.append(&gtk::Label::builder().label("Internet headers").xalign(0.0).css_classes(["heading"]).build());
    let headers_view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    // Seed the header block from the body cache when the message has already
    // been read this session; the raw fetch overwrites it with the
    // authoritative block when it lands.
    let cached_headers = state.borrow_mut().cached_body(&summary.mailbox, summary.uid).map(|body| body.headers);
    headers_view.buffer().set_text(&match &cached_headers {
        Some(headers) => format_header_block(headers),
        None => "Loading…".to_string(),
    });
    let headers_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .min_content_height(200)
        .child(&headers_view)
        .build();
    details_content.append(&headers_scroller);
    let details_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&details_content)
        .build();
    stack.add_titled(&details_scroller, Some("details"), "Details");

    // --- Source page: the raw message, with loading / error states. ---
    let source_view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    let source_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&source_view)
        .build();
    let loading = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .vexpand(true)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .build();
    let spinner = gtk::Spinner::new();
    spinner.start();
    loading.append(&spinner);
    loading.append(&gtk::Label::builder().label("Loading message source…").css_classes(["dim-label"]).build());
    let error_label = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).css_classes(["dim-label"]).build();
    let retry_button = gtk::Button::with_label("Retry");
    retry_button.add_css_class("suggested-action");
    let error_page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .vexpand(true)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .build();
    error_page.append(&error_label);
    error_page.append(&retry_button);
    let status_stack = gtk::Stack::new();
    status_stack.set_transition_type(gtk::StackTransitionType::None);
    status_stack.add_named(&loading, Some("loading"));
    status_stack.add_named(&source_scroller, Some("content"));
    status_stack.add_named(&error_page, Some("error"));
    status_stack.set_visible_child_name("loading");
    stack.add_titled(&status_stack, Some("source"), "Source");
    switcher.set_stack(Some(&stack));
    stack.set_vexpand(true);
    stack.set_visible_child_name(page.name());

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    content.append(&header);
    content.append(&stack);
    window.set_content(Some(&content));

    {
        let window = window.clone();
        close_button.connect_clicked(move |_| window.close());
    }
    {
        // Esc closes the dialog, like every other dialog in the app.
        let keys = gtk::EventControllerKey::new();
        {
            let window = window.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    window.close();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        window.add_controller(keys);
    }
    {
        let state = state.clone();
        let mailbox = summary.mailbox.clone();
        let uid = summary.uid;
        let status_stack = status_stack.clone();
        let error_label = error_label.clone();
        retry_button.connect_clicked(move |_| {
            if send_fetch_raw_message(&state, &mailbox, uid) {
                status_stack.set_visible_child_name("loading");
            } else {
                error_label.set_text("The message source can't be loaded while the account is offline.");
                status_stack.set_visible_child_name("error");
            }
        });
    }
    {
        let state = state.clone();
        let window_for_clear = window.clone();
        window.connect_close_request(move |_| {
            let mut st = state.borrow_mut();
            if st.message_details.as_ref().is_some_and(|d| d.window == window_for_clear) {
                st.message_details = None;
            }
            glib::Propagation::Proceed
        });
    }
    state.borrow_mut().message_details = Some(MessageDetailsDialog {
        window: window.clone(),
        mailbox: summary.mailbox.clone(),
        uid: summary.uid,
        fetched: false,
        status_stack: status_stack.clone(),
        headers_view: headers_view.clone(),
        source_view: source_view.clone(),
        error_label: error_label.clone(),
        pages: stack.clone(),
    });
    if !send_fetch_raw_message(state, &summary.mailbox, summary.uid) {
        error_label.set_text("The message source can't be loaded while the account is offline.");
        status_stack.set_visible_child_name("error");
        if cached_headers.is_none() {
            headers_view.buffer().set_text("The message headers can't be loaded while the account is offline.");
        }
    } else {
        // Backstop: a fetch lost to a reconnect must not leave the dialog on
        // its spinner forever. Only fires while this exact dialog still owns
        // the request and nothing has filled it.
        let state = state.clone();
        let mailbox = summary.mailbox.clone();
        let uid = summary.uid;
        let headers_view = headers_view.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(ATTACHMENT_FETCH_TIMEOUT_MS), move || {
            let mut st = state.borrow_mut();
            let stale = st.message_details.as_ref().is_some_and(|d| d.mailbox == mailbox && d.uid == uid && !d.fetched);
            if stale {
                if let Some(d) = st.message_details.as_mut() {
                    d.error_label.set_text("Loading the message source timed out.");
                    d.status_stack.set_visible_child_name("error");
                }
                headers_view.buffer().set_text("Loading the message headers timed out.");
            }
        });
    }
    window.present();
}

/// Routes a fetched raw message to the "Message details" dialog (item 1.10)
/// when one is open for exactly this message. Arrivals for anything else are
/// ignored, so a dialog closed (or replaced) mid-fetch never shows stale
/// bytes.
pub(crate) fn deliver_raw_to_details_dialog(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid, bytes: &[u8]) {
    let matching = {
        let st = state.borrow();
        st.message_details.as_ref().is_some_and(|d| d.mailbox == *mailbox && d.uid == uid)
    };
    if !matching {
        return;
    }
    let headers = lookout_mail::body::parse_headers_section(bytes);
    let source = message_source_text(bytes);
    let mut st = state.borrow_mut();
    let Some(d) = st.message_details.as_mut() else { return };
    d.headers_view.buffer().set_text(&format_header_block(&headers));
    d.source_view.buffer().set_text(&source);
    d.status_stack.set_visible_child_name("content");
    d.fetched = true;
}

/// Shows the failure page of the "Message details" dialog for a failed (or
/// timed-out) raw fetch, when the dialog still belongs to this message.
pub(crate) fn deliver_raw_error_to_details_dialog(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid, message: &str) {
    let mut st = state.borrow_mut();
    let Some(d) = st.message_details.as_mut() else { return };
    if d.mailbox != *mailbox || d.uid != uid {
        return;
    }
    d.error_label.set_text(&format!("The message source couldn't be loaded: {message}"));
    d.status_stack.set_visible_child_name("error");
    d.fetched = true;
}

/// The key fields shown at the top of the details dialog. Built from the
/// summary so the dialog has content the instant it opens - the raw source
/// may still be in flight.
pub(crate) fn message_detail_fields(summary: &EmailSummary) -> Vec<(String, String)> {
    let addresses = |list: &[EmailAddress]| list.iter().map(|a| a.display_label()).collect::<Vec<_>>().join(", ");
    let mut fields = vec![("From".to_string(), addresses(&summary.from)), ("To".to_string(), addresses(&summary.to))];
    if !summary.cc.is_empty() {
        fields.push(("Cc".to_string(), addresses(&summary.cc)));
    }
    fields.push(("Subject".to_string(), summary.subject.clone().unwrap_or_else(|| "(no subject)".to_string())));
    fields.push(("Date".to_string(), summary.date.with_timezone(&chrono::Local).format("%a %d/%m/%Y %I:%M %p").to_string()));
    if let Some(message_id) = &summary.message_id {
        fields.push(("Message ID".to_string(), message_id.clone()));
    }
    fields
}

/// One caption/value row of the details dialog's key-fields block. The value
/// is selectable so an address or Message-ID can be copied out.
pub(crate) fn message_detail_row(caption: &str, value: &str) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    let caption_label = gtk::Label::builder()
        .label(caption)
        .xalign(0.0)
        .width_request(96)
        .css_classes(["dim-label"])
        .valign(gtk::Align::Start)
        .build();
    let value_label = gtk::Label::builder().label(value).xalign(0.0).wrap(true).selectable(true).hexpand(true).build();
    row.append(&caption_label);
    row.append(&value_label);
    row
}

/// The details dialog's header texts, one `Name: value` line each.
pub(crate) fn format_header_block(headers: &[(String, String)]) -> String {
    headers.iter().map(|(name, value)| format!("{name}: {value}")).collect::<Vec<_>>().join("\n")
}

/// The Source page's text for a raw message: the bytes decoded lossily, or a
/// truncated view with a trailing notice past `MESSAGE_SOURCE_DISPLAY_CAP`.
pub(crate) fn message_source_text(bytes: &[u8]) -> String {
    if bytes.len() <= MESSAGE_SOURCE_DISPLAY_CAP {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut text = String::from_utf8_lossy(&bytes[..MESSAGE_SOURCE_DISPLAY_CAP]).into_owned();
    if let Some(last_newline) = text.rfind('\n') {
        text.truncate(last_newline);
    }
    text.push_str(&format!(
        "\n\n… truncated: showing the first {MESSAGE_SOURCE_DISPLAY_CAP} of {} bytes. Use \"Save as .eml…\" for the full message.",
        bytes.len()
    ));
    text
}

/// RFC 8058 one-click unsubscribe: POST `List-Unsubscribe=One-Click` to the
/// list's URL. Success is a 2xx (reqwest follows the redirects some lists
/// answer with); a non-2xx response or a transport failure is an error the
/// caller surfaces as a toast (and may fall back to the mailto action).
pub(crate) async fn post_one_click_unsubscribe(url: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    let response = client
        .post(url)
        .form(&[("List-Unsubscribe", "One-Click")])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("the list answered {}", response.status()))
    }
}

/// Opens the composer pre-filled to send the list's unsubscribe mail - the
/// RFC 2369 `mailto:` fallback for lists without one-click POST (and the
/// fallback when a one-click POST fails). Pre-fills only the recipient and
/// subject, so the user reviews what goes out before anything is sent.
pub(crate) fn open_mailto_unsubscribe(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
    address: String,
    cmd_tx: async_channel::Sender<AccountCommand>,
    account_id: Option<AccountId>,
) {
    let prefill = crate::compose::ComposePrefill {
        to: Some(address),
        subject: Some("unsubscribe".to_string()),
        ..Default::default()
    };
    let rich_text_default = state.borrow().rich_text_default;
    show_composer_in_reading_pane(
        state,
        worker,
        reading_stack,
        "Unsubscribe",
        cmd_tx,
        prefill,
        rich_text_default,
        account_id,
        Some(crate::config_view::SignatureDefault::Reply),
    );
}

/// The zoom limits and step for Ctrl+scroll / Ctrl+± on a WebKit view.
pub(crate) const WEB_VIEW_MIN_ZOOM: f64 = 0.5;

pub(crate) const WEB_VIEW_MAX_ZOOM: f64 = 3.0;

pub(crate) const WEB_VIEW_ZOOM_STEP: f64 = 0.1;

/// Ctrl+scroll and Ctrl+= / Ctrl+- / Ctrl+0 zoom on a WebKit view (item 1.20 /
/// M-37). Attached to the view itself rather than the main window's shortcut
/// controller, so it works in the reading pane, the popped-out message window
/// and the composer - and only while that view has focus.
pub(crate) fn install_web_view_zoom(web_view: &webkit::WebView) {
    let key = gtk::EventControllerKey::new();
    {
        let web_view = web_view.clone();
        key.connect_key_pressed(move |_controller, keyval, _keycode, state| {
            use gtk::gdk::Key;
            if !state.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                return glib::Propagation::Proceed;
            }
            let current = web_view.zoom_level();
            let next = match keyval {
                Key::equal | Key::plus | Key::KP_Add => current + WEB_VIEW_ZOOM_STEP,
                Key::minus | Key::KP_Subtract => current - WEB_VIEW_ZOOM_STEP,
                Key::_0 | Key::KP_0 => 1.0,
                _ => return glib::Propagation::Proceed,
            };
            web_view.set_zoom_level(next.clamp(WEB_VIEW_MIN_ZOOM, WEB_VIEW_MAX_ZOOM));
            glib::Propagation::Stop
        });
    }
    web_view.add_controller(key);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    {
        let web_view = web_view.clone();
        scroll.connect_scroll(move |controller, _dx, dy| {
            if dy == 0.0 || !controller.current_event_state().contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                return glib::Propagation::Proceed;
            }
            let current = web_view.zoom_level();
            let next = if dy < 0.0 { current + WEB_VIEW_ZOOM_STEP } else { current - WEB_VIEW_ZOOM_STEP };
            web_view.set_zoom_level(next.clamp(WEB_VIEW_MIN_ZOOM, WEB_VIEW_MAX_ZOOM));
            glib::Propagation::Stop
        });
    }
    web_view.add_controller(scroll);
}

/// Arms or disarms the "Switch message theme" override's *physical* state on
/// the reading pane's WebView: the user stylesheet that strips the message's
/// backgrounds and inverts its colours, plus the canvas colour (transparent
/// so the app theme shows through while armed, white otherwise, matching
/// WebKit's default). Must be called whenever the logical per-message state
/// (`UiState::message_theme_override`) changes, so the two never drift - the
/// toggle handler, every navigation reset, and the close-pane reset all
/// route through this.
pub(crate) fn set_message_theme_armed(enabled: bool, web_view: &webkit::WebView, user_content_manager: &webkit::UserContentManager, theme_override_sheet: &webkit::UserStyleSheet) {
    if enabled {
        user_content_manager.add_style_sheet(theme_override_sheet);
        web_view.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    } else {
        user_content_manager.remove_style_sheet(theme_override_sheet);
        web_view.set_background_color(&gtk::gdk::RGBA::new(1.0, 1.0, 1.0, 1.0));
    }
}

/// Re-renders the message currently selected in the message list onto the
/// reading pane, routing through "empty" first so `render_body`'s
/// already-shown guard doesn't treat the reload of the same message as a
/// no-op. Used by the Config → Mail "Load images" toggle and the
/// external-content banner's actions, which both change the load policy
/// for the open message and must apply to it rather than the next
/// selection. Skipped while a composer is up - `render_body` would route
/// the pane back to the message page and yank the user out of their draft.
pub(crate) fn rerender_current_message(
    state: &Rc<RefCell<UiState>>,
    reading_stack: &gtk::Stack,
    message_header: &crate::message_header::MessageHeader,
    message_list: &crate::message_list::MessageListModel,
) {
    if reading_stack.visible_child_name().as_deref() == Some("compose") {
        return;
    }
    let (mailbox, uid, body) = {
        let mut st = state.borrow_mut();
        let Some(summary) = message_list.selected_summary() else { return };
        let Some(body) = st.body_cache.get(&summary.mailbox, &summary.uid) else { return };
        // Stash the summary for the fresh render: `render_body` re-derives
        // the reading-pane header and the rendered sender (which the load
        // policy and the trust banner consult) from it.
        let mailbox = summary.mailbox.clone();
        let uid = summary.uid;
        st.pending_header = Some(summary);
        (mailbox, uid, body)
    };
    // Drop `rendered_message` and route through "empty" first, or
    // `render_body`'s already-shown guard would treat the reload of the
    // same message as a no-op and never re-issue the `load_html`.
    reading_stack.set_visible_child_name("empty");
    state.borrow_mut().rendered_message = None;
    render_body(state, reading_stack, message_header, mailbox, uid, body);
}

/// Strips the angle brackets an `In-Reply-To`/`References` value must not
/// carry (the composer prefill's convention: `mail_builder`'s MessageId
/// writer adds them itself).
pub(crate) fn bare_message_id(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed.strip_prefix('<').and_then(|r| r.strip_suffix('>')).unwrap_or(trimmed).to_string()
}

/// Sends the RFC 8098 read receipt for the message currently on the reading
/// pane - the `Disposition-Notification-To` request and the original-message
/// context stashed by `render_body`. `automatic` picks the report's
/// disposition mode (`automatic-action/MDN-sent-automatically` vs
/// `manual-action/MDN-sent-manually`). Returns whether the receipt was
/// queued; on success the message is marked receipted (the banner won't
/// offer it again) and a toast confirms. Both callers - the banner's Send
/// button and the automatic policy - route through this, so the receipt is
/// built and sent one way. The send itself is the same `SendMessage` path
/// the composer uses (SMTP + a copy in Sent), from the receiving account's
/// own address so the receipt is legitimate.
pub(crate) fn send_read_receipt(state: &Rc<RefCell<UiState>>, automatic: bool) -> bool {
    let (mailbox, uid, request, context) = {
        let st = state.borrow();
        let (mailbox, uid) = match &st.rendered_message {
            Some(rendered) => rendered.clone(),
            None => return false,
        };
        let Some(request) = st.read_receipt_request.clone() else { return false };
        let Some(context) = st.read_receipt_context.clone() else { return false };
        (mailbox, uid, request, context)
    };
    let Some(account_id) = mailbox_account_id(&mailbox) else { return false };
    let account_lookup = {
        let st = state.borrow();
        st.accounts
            .get(&account_id)
            .map(|handle| (handle.email.clone(), handle.display_name.clone(), handle.cmd_tx.clone()))
    };
    let Some((email, display_name, cmd_tx)) = account_lookup else {
        return false;
    };
    let Some(to) = request.first() else { return false };
    let original_message_id = context.message_id;
    let bare_id = bare_message_id(&original_message_id);
    let subject = if context.subject.trim().is_empty() {
        "Read receipt".to_string()
    } else {
        format!("Read receipt: {}", context.subject.trim())
    };
    let message = lookout_mail::ComposedMessage {
        from: email.clone(),
        display_name: Some(display_name).filter(|name| !name.trim().is_empty()),
        to: vec![to.clone()],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        subject,
        // The report branch of `build_raw_message` replaces the text body
        // with the human-readable part it builds itself.
        text_body: String::new(),
        html_body: None,
        attachments: vec![],
        inline_images: vec![],
        calendar_part: None,
        read_receipt: Some(lookout_mail::ReadReceipt {
            original_message_id,
            original_from: context.original_from,
            original_subject: context.subject,
            original_date: context.date,
            final_recipient: email,
            automatic,
            displayed_at: chrono::Local::now().format("%a, %e %b %Y %H:%M %z").to_string(),
            original_headers: context.headers,
        }),
        request_read_receipt: false,
        importance: lookout_core::Importance::Normal,
        sensitivity: lookout_core::MessageSensitivity::Normal,
        in_reply_to: Some(bare_id.clone()),
        references: vec![bare_id],
        message_id: None,
    };
    if cmd_tx
        .send_blocking(AccountCommand::SendMessage {
            message: Box::new(message),
            not_before: None,
            client_token: None,
        })
        .is_err()
    {
        return false;
    }
    {
        let mut st = state.borrow_mut();
        st.read_receipts_sent.insert((mailbox.clone(), uid));
        st.read_receipt_dismissed = Some((mailbox, uid));
        if let Some(toast_overlay) = &st.toast_overlay {
            toast_overlay.add_toast(adw::Toast::new("Read receipt sent"));
        }
    }
    true
}

/// The Content-Security-Policy prepended to every message body before it's
/// loaded into the reading pane, enforcing the external-content load policy
/// in the web process. WebKitGTK only delivers `decide-policy` response
/// decisions for the main frame - not for subresources (verified empirically
/// against 2.52) - so the response veto in the reading pane's policy handler
/// can't gate remote images; CSP is checked on every resource load, cache
/// hits included, and the meta tag below - prepended before any other
/// element (or slipped after the doctype, so a doctyped message stays in
/// standards mode), so the HTML5 parser hoists it into the head even for
/// fragment bodies - is the block that actually holds.
///
/// `images_allowed` mirrors the "Load images from the web" toggle, the
/// per-message "load once" override, and a sender trusted at the `Images`
/// level: remote `image/*` loads pass, everything else remote stays blocked.
/// `all_allowed` is a sender trusted at the `AllContent` level: every remote
/// subresource passes. Either way the message's own inline `data:`/`cid:`
/// content always loads, JavaScript stays off (`script-src 'none'`, also
/// enforced by the WebKit setting), and `<frame>`/`<object>`/`<base>`/form
/// targets are vetoed so a trusted sender still never gains navigation or
/// scripting.
pub(crate) fn message_content_security_policy(images_allowed: bool, all_allowed: bool) -> &'static str {
    if all_allowed {
        "default-src * data: cid: blob:; style-src 'unsafe-inline' * data: cid:; script-src 'none'; frame-src 'none'; object-src 'none'; connect-src * data:; base-uri 'none'; form-action 'none'"
    } else if images_allowed {
        "default-src 'none'; img-src * data: cid:; style-src 'unsafe-inline'; font-src data: cid:; media-src data: cid:; frame-src 'none'; object-src 'none'; connect-src 'none'; script-src 'none'; base-uri 'none'; form-action 'none'"
    } else {
        "default-src 'none'; img-src data: cid:; style-src 'unsafe-inline'; font-src data: cid:; media-src data: cid:; frame-src 'none'; object-src 'none'; connect-src 'none'; script-src 'none'; base-uri 'none'; form-action 'none'"
    }
}

/// Prepend the reading pane's Content-Security-Policy meta tag to a message
/// body, keeping it ahead of every content element the CSP spec counts: in
/// front of the whole body for the usual fragment HTML, or slipped in right
/// after the opening doctype when one leads the body, so a doctyped message
/// keeps its standards-mode rendering (a meta before the doctype would trip
/// the HTML5 parser into quirks mode and the doctype would then be ignored).
/// `pub(crate)` because the Lookout tab's chat output loads through the same
/// wrapper (`images_allowed`, never `all_allowed`, so a reply may hotlink
/// images but nothing else remote).
pub(crate) fn wrap_message_with_csp(html: &str, images_allowed: bool, all_allowed: bool) -> String {
    let csp_tag = format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">",
        message_content_security_policy(images_allowed, all_allowed)
    );
    let lead = html.len() - html.trim_start().len();
    let trimmed = &html[lead..];
    let mut out = String::with_capacity(html.len() + csp_tag.len() + 1);
    if trimmed.as_bytes().get(..9).is_some_and(|b| b.eq_ignore_ascii_case(b"<!doctype")) {
        if let Some(end) = trimmed.find('>') {
            out.push_str(&html[..lead + end + 1]);
            out.push_str(&csp_tag);
            out.push_str(&html[lead + end + 1..]);
            return out;
        }
    }
    out.push_str(&csp_tag);
    out.push_str(&html[..lead]);
    out.push_str(trimmed);
    out
}

/// Resolves the external-content load policy for a message: whether remote
/// `image/*` subresources may load (`images_allowed`) and whether every
/// remote subresource may (`all_allowed`). The inputs are the per-account
/// "Load images from the web" preference, the transient load-once overrides
/// (`state`'s for the reading pane, `load_images_once`'s for a pop-out
/// message window), and the sender's trust entry for the receiving account.
/// `trust_sender` is the `(account, normalized sender address)` pair to
/// resolve against `trusted_senders` - `None` when there's no sender to key
/// trust on (the debug `.eml` viewer). Shared between `render_body` and the
/// pop-out message window (item 1.9) so both apply the identical policy.
pub(crate) fn remote_content_policy(state: &Rc<RefCell<UiState>>, trust_sender: Option<&(AccountId, String)>, load_images_once: bool) -> (bool, bool) {
    let st = state.borrow();
    let level = trust_sender.and_then(|(account, sender)| st.trusted_senders.get(&(account.clone(), sender.clone())).copied());
    let images_allowed = st.load_remote_images || st.load_once_images || load_images_once || level.is_some_and(|l| l >= lookout_core::TrustLevel::Images);
    let all_allowed = level.is_some_and(|l| l >= lookout_core::TrustLevel::AllContent);
    (images_allowed, all_allowed)
}

/// Whether `(mailbox, uid)` is on screen in any surface: the reading pane
/// (`rendered_message`) or an open "message in new window" pop-out (item
/// 1.9). The attachment Open/Save flows use this instead of a bare
/// `rendered_message` check so a fetch started from a pop-out completes even
/// after the reading pane has moved to a different message.
pub(crate) fn message_is_visible(st: &UiState, mailbox: &MailboxId, uid: Uid) -> bool {
    st.rendered_message.as_ref() == Some(&(mailbox.clone(), uid))
        || st.message_windows.contains_key(&(mailbox.clone(), uid))
        || st
            .rendered_conversation
            .as_ref()
            .is_some_and(|pane| pane.cards.iter().any(|card| card.member.summary.mailbox == *mailbox && card.member.summary.uid == uid))
}

/// Dispatches `FetchRawMessage` for `(mailbox, uid)` on its account's
/// interactive channel - the shared first step of the .eml export, the
/// "Message details" dialog (item 1.10) and the message window (item 1.9).
/// Silent no-op returning `false` when the mailbox's account isn't connected.
pub(crate) fn send_fetch_raw_message(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid) -> bool {
    let Some(account_id) = mailbox_account_id(mailbox) else { return false };
    let cmd_tx = state.borrow().accounts.get(&account_id).map(|h| h.interactive_cmd_tx.clone());
    match cmd_tx {
        Some(cmd_tx) => cmd_tx.send_blocking(AccountCommand::FetchRawMessage { mailbox: mailbox.clone(), uid }).is_ok(),
        None => false,
    }
}

// --- Conversation view (item 1.7, M-04 / UX-35) ---------------------------
//
// The reading pane's "conversation" page: every message of the selected
// conversation stacked as cards in one WebView (`conversation_view` builds
// the document and holds the trusted script). It lives beside the single-
// message "message" page rather than replacing it - a lone message, any
// message while conversations are off, and "Show details" on a card still
// render there, with every banner it has.

/// Opens an `http(s)` URI in the system's default browser - a link clicked
/// in the reading pane's message or conversation views. Other schemes are
/// deliberately ignored - `data:`/`cid:`/`about:`/`file:` are local to the
/// pane, and `mailto:` is itself a mail client's business (a
/// compose-from-link flow is the later refinement).
pub(crate) fn open_uri_in_default_browser(uri: &str) {
    let scheme = uri.split(':').next().unwrap_or("");
    if matches!(scheme, "http" | "https") {
        if let Err(e) = gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>) {
            tracing::warn!("failed to open link {uri} in the default browser: {e}");
        }
    }
}

/// Confirms before opening a link whose visible text named a different host
/// (item 6.3 / S-08) - the message body's text can't be trusted to state the
/// real destination. Opens in the default browser only on "Open".
pub(crate) fn show_suspicious_link_dialog(parent: &impl IsA<gtk::Widget>, uri: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading("Open this link?")
        .body(format!("The link's text doesn't match where it actually goes:\n\n{uri}"))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("open", "Open");
    dialog.set_response_appearance("open", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let uri = uri.to_string();
    dialog.connect_response(None, move |_, response| {
        if response == "open" {
            open_uri_in_default_browser(&uri);
        }
    });
    dialog.present(Some(parent));
}

/// The sender-authentication banner's title (item 6.1 / S-03, M-35), or `None`
/// when there is nothing to warn about. A display-name/address mismatch is the
/// most specific cue; otherwise a definite SPF/DKIM/DMARC failure warns, and a
/// set of results with no pass at all (e.g. all `none`) reads as unverified.
/// Returns `None` when there are no results and no mismatch.
pub(crate) fn auth_banner_title(results: Option<&lookout_core::AuthenticationResults>, mismatch: Option<&str>) -> Option<String> {
    if let Some(impersonated) = mismatch {
        return Some(format!("The sender's name includes {impersonated}, which may not match their address"));
    }
    let results = results?;
    if results.any_failure() {
        Some("This message may not be from the sender it claims to be".to_string())
    } else if !results.any_pass() {
        Some("The sender of this message could not be verified".to_string())
    } else {
        None
    }
}

pub(crate) fn render_body(
    state: &Rc<RefCell<UiState>>,
    reading_stack: &gtk::Stack,
    message_header: &crate::message_header::MessageHeader,
    mailbox: MailboxId,
    uid: Uid,
    body: lookout_core::EmailBody,
) {
    // Defense-in-depth for the direct `BodyFetched` caller: if this exact
    // message is already on screen, a duplicate body event must not route the
    // pane through "empty" and crossfade the same email again. The selection
    // handler's `already_shown` guard covers the normal path; this covers a
    // stray re-fetch racing a repopulate. (The selection handler's cached
    // path passes through "empty" before calling this, so a fresh render of
    // the newly-selected message still lands here.)
    let already_shown = {
        let st = state.borrow();
        st.rendered_message.as_ref() == Some(&(mailbox.clone(), uid)) && reading_stack.visible_child_name().as_deref() == Some("message")
    };
    if already_shown {
        return;
    }
    // Remember what's being rendered so a later re-selection of the same
    // message (see the selection handler's `already_shown` guard) can be
    // recognized as a no-op instead of another crossfade.
    state.borrow_mut().rendered_message = Some((mailbox.clone(), uid));
    // The body arrived: drop the pending request marker and disable any
    // still-armed failure backstop, and stop the loading spinner. Without this
    // the timeout could fire after a successful (if slow) fetch and show the
    // unavailable page over an already-rendered message.
    clear_body_fetch_state(state, reading_stack);
    // The trust banner's `html_remote_content_scan` re-parses the message's
    // whole HTML, which is unchanged across re-renders of the same message
    // (theme toggle, list repopulate keeping the selection). Compute it once
    // per render into a single-slot cache keyed by this message; the banner
    // block reads the cache instead of rescanning.
    {
        let mut st = state.borrow_mut();
        let cached = st.rendered_remote_scan.as_ref().filter(|(m, u, _)| m == &mailbox && u == &uid).map(|(_, _, s)| *s);
        st.rendered_remote_scan = Some((
            mailbox.clone(),
            uid,
            cached.unwrap_or_else(|| body.html_body.as_deref().map(lookout_core::html_remote_content_scan).unwrap_or_default()),
        ));
    }
    // The pane is about to show a new message: any inline `cid:` image
    // requests still in flight belong to the one being replaced, so finish
    // them with an error (WebKit re-requests if the same images come up
    // again, and the cache serves those). Then stash the new message's
    // `cid:`-bearing parts for the scheme handler to resolve against.
    drop_pending_cid(state);
    state.borrow_mut().rendered_inline_parts = body.parts.iter().filter(|p| p.cid.is_some()).cloned().collect();
    // Link-safety warnings (item 6.3 / S-08) for the message being rendered:
    // the hrefs of anchors whose visible text names a different host. The
    // link-click handler confirms before opening one of these.
    state.borrow_mut().rendered_link_warnings = body.html_body.as_deref().map(crate::link_safety::suspicious_link_hrefs).unwrap_or_default();
    // Apply the header for the message being rendered now. The selection
    // handler stores the summary here instead of updating the header
    // immediately, so the previous message's header stays on screen through
    // its whole fade-out; by the time we get here the pane is already on the
    // "empty" placeholder, so updating the (hidden) header can't flash. The
    // summary's From address is also stashed - normalized - as the rendered
    // sender the load policy resolves against `trusted_senders` and the
    // trust banner shows. The debug `.eml` viewer has no summary (and no
    // account to key trust on anyway), so the sender ends up `None` there
    // and all remote content stays blocked.
    let mut rendered_follow_up = None;
    {
        let mut st = state.borrow_mut();
        // A message opened on its own from the conversation view (item
        // 1.7) offers the way back; any other render hides it.
        message_header.back_button.set_visible(st.rendered_conversation.is_some());
        if let Some(summary) = st.pending_header.take() {
            message_header.update(&summary);
            rendered_follow_up = summary.follow_up().map(|flag| (mailbox.clone(), uid, flag));
            let sender = summary.from.first().map(|a| a.address.trim().to_lowercase());
            st.rendered_trust_sender = mailbox_account_id(&mailbox).zip(sender);
            // Display-name impersonation (6.1 / S-03): a From display name that
            // hides a different address drives the auth banner alongside the
            // server's Authentication-Results.
            st.rendered_from_mismatch = summary.from.first().and_then(lookout_core::display_name_address_mismatch);
            // Draft status (item 1.12): the message-list summary's `\Draft`
            // flag (which also catches drafts surfaced outside the Drafts
            // folder, e.g. search copies) or the mailbox's role. The draft
            // banner below is revealed from this.
            st.rendered_draft = summary_is_draft(&st, &summary).then(|| (mailbox.clone(), uid));
            st.rendered_draft_message_id = st
                .rendered_draft
                .as_ref()
                .and(summary.message_id.as_deref())
                .map(|id| crate::compose::strip_angle_brackets(id).to_string());
            st.rendered_snooze = lookout_core::snooze_until_from_keywords(&summary.keywords).map(|until| (mailbox.clone(), uid, until));
        } else {
            st.rendered_trust_sender = None;
            st.rendered_from_mismatch = None;
            st.rendered_draft = None;
            st.rendered_snooze = None;
            st.rendered_draft_message_id = None;
        }
    }
    // Draft banner (item 1.12): offer "Edit" only for the message actually
    // being rendered, so a stale stash can never act on a different row.
    let is_rendered_draft = state.borrow().rendered_draft.as_ref() == Some(&(mailbox.clone(), uid));
    refresh_draft_banner(state, reading_stack, is_rendered_draft);
    // Snooze bar (item 1.15), for the rendered message only.
    let snoozed_until = state
        .borrow()
        .rendered_snooze
        .as_ref()
        .filter(|(m, u, _)| *m == mailbox && *u == uid)
        .map(|(_, _, until)| *until);
    if let Some(banner) = find_named_child(reading_stack, "snooze-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
        if let Some(until) = snoozed_until {
            let when = crate::datetime_picker::format_short_time(&until.with_timezone(&chrono::Local), &chrono::Local::now());
            banner.set_title(&format!("Snoozed until {when}"));
        }
        banner.set_revealed(snoozed_until.is_some());
    }
    // Follow-up bar (item 1.16 / UX-33), for the rendered message only.
    show_follow_up_banner(state, reading_stack, rendered_follow_up);
    // Sync the "Switch message theme" toggle with its per-email state: the
    // selection handler resets `message_theme_override` to the configured
    // default on every navigation (with the physical sheet/canvas already
    // re-armed to match), so this flips the header button accordingly for
    // the next message. A no-op `set_active` (same value) emits no `toggled`
    // signal, so this can't feed back into the toggle's handler or its
    // re-render.
    message_header.theme_button.set_active(state.borrow().message_theme_override);
    // Rebuild the attachment strip from the body's part list; the body is
    // available regardless of which text path (html/text/none) renders below.
    rebuild_attachment_strip(state, reading_stack, &mailbox, uid, &body.parts);
    // List-Unsubscribe banner: stash the rendered message's parsed
    // unsubscribe actions for the banner's button handler, and reveal the
    // banner when the message offers an action the user hasn't dismissed for
    // this message (the close button records the dismissal). Hidden
    // otherwise - and on every navigation, since the selection handler
    // clears `unsubscribe_dismissed`.
    {
        let mut st = state.borrow_mut();
        st.unsubscribe_info = lookout_core::parse_list_unsubscribe(&body.headers);
        let dismissed = st.unsubscribe_dismissed.as_ref() == Some(&(mailbox.clone(), uid));
        if let Some(banner) = find_named_child(reading_stack, "unsubscribe-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            banner.set_revealed(!dismissed && st.unsubscribe_info.is_some());
        }
    }
    // iMIP banner: parse the rendered message's `text/calendar` payload (if
    // any) into the invitation the banner's button handler acts on, and
    // reveal the banner when there's one the user hasn't dismissed for this
    // message. The per-method button label/title is set here too - it varies
    // with what the payload asks of the user. Hidden otherwise - and on every
    // navigation, since the selection handler clears `imip_dismissed`.
    {
        let mut st = state.borrow_mut();
        st.imip = body.calendar_ics.as_deref().and_then(lookout_dav::parse_imip_invitation);
        if let Some(invitation) = st.imip.as_mut() {
            // The reply's In-Reply-To is the invitation message's own
            // Message-ID, which lives in the message headers rather than the
            // iCalendar document.
            invitation.in_reply_to = lookout_core::header_value(&body.headers, "message-id").map(str::to_string);
        }
        let dismissed = st.imip_dismissed.as_ref() == Some(&(mailbox.clone(), uid));
        if let Some(banner) = find_named_child(reading_stack, "imip-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            let (title, button) = match (&st.imip, dismissed) {
                (Some(invitation), false) => match invitation.method {
                    lookout_core::ImipMethod::Request => (format!("Invitation: {}", invitation.summary.as_deref().unwrap_or("an event")), "Respond…".to_string()),
                    lookout_core::ImipMethod::Cancel => (
                        format!("Cancelled: {}", invitation.summary.as_deref().unwrap_or("an event")),
                        "Remove from calendar".to_string(),
                    ),
                    lookout_core::ImipMethod::Reply => (format!("RSVP update: {}", invitation.summary.as_deref().unwrap_or("an event")), "Dismiss".to_string()),
                    lookout_core::ImipMethod::Counter => (format!("New time proposed: {}", invitation.summary.as_deref().unwrap_or("an event")), "Review…".to_string()),
                    lookout_core::ImipMethod::DeclineCounter => (format!("Proposal declined: {}", invitation.summary.as_deref().unwrap_or("an event")), "Dismiss".to_string()),
                },
                _ => (String::new(), String::new()),
            };
            banner.set_title(&title);
            banner.set_button_label(Some(&button));
            banner.set_revealed(!dismissed && st.imip.is_some());
            // The invite-details card mirrors the banner's visibility: shown
            // for the same payload, hidden once the user dismisses it (or
            // when the message carries no invitation at all - `st.imip` was
            // set from this message's `text/calendar` part above).
            let invitation = st.imip.clone().filter(|_| !dismissed);
            let (conflicts, timeline) = match &invitation {
                Some(invitation) => {
                    let prefs = st.settings.calendar_preferences();
                    invite_card_extras(&st.calendar_state, &prefs, invitation)
                }
                None => (Vec::new(), None),
            };
            render_invite_card(reading_stack, invitation.as_ref(), &conflicts, timeline);
        }
    }
    // An incoming REPLY (METHOD:REPLY) updates the organizer's stored event
    // the first time its message is rendered (item 3.6 / CAL-16). Runs after
    // the block above so its `RefCell` borrow is released; the applied set
    // keeps a re-render from merging and PUTting the same reply twice.
    {
        let reply = state
            .borrow()
            .imip
            .as_ref()
            .filter(|invitation| invitation.method == lookout_core::ImipMethod::Reply)
            .cloned();
        if let Some(invitation) = reply {
            let first = state.borrow_mut().imip_replies_applied.insert((mailbox.clone(), uid));
            if first {
                if let Some(calendar_state) = state.borrow().calendar_state.clone() {
                    apply_incoming_imip_reply(state, &calendar_state, &invitation);
                }
            }
        }
    }
    // External-content load policy for this render: whether the message's
    // remote content may load is decided once here, from the same inputs the
    // reading pane's decide-policy handler uses - the rendered sender's
    // trust entry (per receiving account) and the global "Load images from
    // the web" toggle / per-message "load once" override. The result feeds
    // the trust banner's reveal below and the Content-Security-Policy
    // injected into the loaded HTML; because WebKitGTK only delivers
    // `decide-policy` response decisions for the main frame (not for
    // subresources - verified empirically against 2.52), the CSP is what
    // actually enforces the block on every remote image, style, font and
    // media load.
    let trust_sender = state.borrow().rendered_trust_sender.clone();
    let (images_allowed, all_allowed) = remote_content_policy(state, trust_sender.as_ref(), false);
    // Trust banner: reveal it when the message on screen references remote
    // content the policy above is *currently blocking* - its sender isn't
    // trusted (at a high enough level) and the global "Load images from the
    // web" toggle isn't covering the gap. The HTML scan is advisory; the CSP
    // injected below is authoritative. The sender comes from
    // `rendered_trust_sender`, stashed above; hidden when there's no sender
    // to key trust on (the debug `.eml` viewer).
    {
        let st = state.borrow();
        let dismissed = st.trust_banner_dismissed.as_ref() == Some(&(mailbox.clone(), uid));
        if let Some(banner) = find_named_child(reading_stack, "trust-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            match st.rendered_trust_sender.clone() {
                // No sender to key trust on (the debug `.eml` viewer): the
                // banner has nothing to act on, so leave it hidden.
                None => banner.set_revealed(false),
                Some((_, sender)) => {
                    // `html_remote_content_scan` re-parses the whole rendered
                    // HTML; `render_body` computed it once per render and
                    // cached it keyed by this message (see below), so a
                    // re-render of the same message reuses the scan instead of
                    // rescanning.
                    let scan = st
                        .rendered_remote_scan
                        .as_ref()
                        .filter(|(m, u, _)| m == &mailbox && u == &uid)
                        .map(|(_, _, s)| *s)
                        .unwrap_or_default();
                    let images_blocked = scan.has_images && !images_allowed;
                    let other_blocked = scan.has_other && !all_allowed;
                    banner.set_title(&format!("Remote content from {sender} is blocked"));
                    banner.set_revealed(!dismissed && (images_blocked || other_blocked));
                }
            }
        }
    }
    // Sender-authentication banner (item 6.1 / S-03, M-35): a definite
    // SPF/DKIM/DMARC failure, an unverifiable sender, or a display name that
    // hides a different address. Read from the body's parsed
    // `Authentication-Results` and the mismatch stashed from the summary.
    {
        let st = state.borrow();
        if let Some(banner) = find_named_child(reading_stack, "auth-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            match auth_banner_title(body.auth_results.as_ref(), st.rendered_from_mismatch.as_deref()) {
                Some(title) => {
                    banner.set_title(&title);
                    banner.set_revealed(true);
                }
                None => banner.set_revealed(false),
            }
        }
    }
    // External-sender banner (item 6.2 / S-06): the rendered sender's domain
    // is outside the configured internal-domain list. Off when the list is
    // empty, so the tag only ever appears once the user has set it up.
    {
        let st = state.borrow();
        let domains = st.settings.get_strv(crate::settings::MAIL_INTERNAL_DOMAINS);
        if let Some(banner) = find_named_child(reading_stack, "external-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            let external = st
                .rendered_trust_sender
                .as_ref()
                .is_some_and(|(_, sender)| lookout_core::is_external_sender(sender, &domains));
            banner.set_revealed(external);
        }
    }
    // Sensitivity banner (item 6.5 / C-06): the rendered message's RFC 2156
    // `Sensitivity`. Normal is left off the wire, so the banner only appears
    // for Personal / Private / Company-Confidential.
    {
        let sensitivity = lookout_core::sensitivity_from_headers(&body.headers).unwrap_or_default();
        if let Some(banner) = find_named_child(reading_stack, "sensitivity-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            if let Some(label) = sensitivity.as_header_value() {
                banner.set_title(&format!("Sensitivity: {label}"));
                banner.set_revealed(true);
            } else {
                banner.set_revealed(false);
            }
        }
    }
    // Read-receipt request (RFC 8098 `Disposition-Notification-To`): stash
    // the rendered message's requested addresses - and the original-message
    // context a receipt needs - for the banner's button handler and the
    // automatic policy. Under the automatic policy the receipt goes out
    // immediately (once per message per session) with a toast instead of a
    // banner; otherwise the banner asks. Neither happens for
    // machine-generated mail (RFC 3834) or reports (RFC 8098 §2.1.4) - a
    // receipt for a receipt is how loops start - and never for the debug
    // `.eml` viewer, which has no account to send from.
    {
        let request = lookout_core::parse_disposition_notification_to(&body.headers);
        let mut st = state.borrow_mut();
        st.read_receipt_request = (!request.is_empty()).then_some(request);
        st.read_receipt_context = st.read_receipt_request.as_ref().map(|_| ReadReceiptContext {
            message_id: lookout_core::header_value(&body.headers, "message-id").unwrap_or("").to_string(),
            original_from: lookout_core::header_value(&body.headers, "from").unwrap_or("").to_string(),
            subject: lookout_core::header_value(&body.headers, "subject").unwrap_or("").to_string(),
            date: lookout_core::header_value(&body.headers, "date").map(str::to_string),
            headers: body.headers.clone(),
        });
        let key = (mailbox.clone(), uid);
        let automatic = st.settings.get_bool(crate::settings::MAIL_SEND_READ_RECEIPTS);
        let eligible = st.read_receipt_request.is_some()
            && !lookout_core::is_auto_submitted(&body.headers)
            && !lookout_core::is_report_message(&body.headers)
            && mailbox_account_id(&mailbox).is_some();
        let already_sent = st.read_receipts_sent.contains(&key);
        if let Some(banner) = find_named_child(reading_stack, "read-receipt-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            banner.set_revealed(eligible && !automatic && !already_sent && st.read_receipt_dismissed.as_ref() != Some(&key));
        }
        drop(st);
        if automatic && eligible && !already_sent {
            send_read_receipt(state, true);
        }
    }
    // Config → Appearance → "Animate transitions" can switch the stack's
    // transition type to `None`; when it's off, skip the fade-specific paths
    // below (routing through "empty", waiting for the WebView to paint) and
    // swap content in directly, matching the pre-fade behavior.
    let animated = reading_stack.transition_type() != gtk::StackTransitionType::None;
    // The "message" page groups the header with the body's content stack
    // (web view / text view), so revealing it - vs. the "empty" page - is
    // what crossfades, carrying the whole header + body together.
    let Some(content_stack) = find_named_child(reading_stack, "body").and_then(|child| child.downcast::<gtk::Stack>().ok()) else {
        return;
    };
    if let Some(html) = &body.html_body {
        // The external-content load policy rides in on a Content-Security-
        // Policy meta tag (see `message_content_security_policy`), slipped in
        // before any content element - or right after a leading doctype so a
        // doctyped message keeps its standards-mode rendering (see
        // `wrap_message_with_csp`).
        let loaded_html = wrap_message_with_csp(html, images_allowed, all_allowed);
        if let Some(web_view) = content_stack.child_by_name("html").and_downcast::<webkit::WebView>() {
            // Drop back to "empty" first, whether or not transitions are
            // animated: the WebView is a single long-lived widget reused for
            // every message, and `load_html` paints asynchronously, so
            // leaving it mapped/visible across the swap would show the
            // previous message's last-painted frame until WebKit catches up
            // (with "Animate transitions" off this was previously
            // deterministic - see the reveal-gating below, which now applies
            // uniformly). GTK also only transitions when the visible child
            // actually changes, so this doubles as the re-render-of-the-
            // same-page case.
            if reading_stack.visible_child_name().as_deref() == Some("message") {
                reading_stack.set_visible_child_name("empty");
            }
            // WebKit paints asynchronously - crossfading into the HTML page
            // while a fresh body is still loading would show a blank/white
            // page before the message appears. So arm the persistent
            // `load-changed` handler (`pending_html_reveal`) and let it
            // reveal the page once the load completes. The selection handler
            // disarms it on every selection change, so a load started for a
            // message the user has already moved on from can never reveal.
            state.borrow_mut().pending_html_reveal = true;
            web_view.load_html(&loaded_html, None);
            tracing::debug!(?mailbox, uid = uid.0, "render_body: load_html issued");
            // The reveal above is gated on WebKit's `Finished` event, but a
            // slow/hung load must not be able to hold the pane on "empty"
            // indefinitely. Arm a fallback that reveals the page after a
            // short grace period if the load hasn't finished. It captures
            // `reveal_generation` at arm time and only reveals if the counter
            // is unchanged, so a stale timeout from a message the user has
            // already left can't pop the next message's page open mid-load
            // (the selection handler bumps the counter when it disarms
            // `pending_html_reveal`).
            let generation = state.borrow().reveal_generation;
            let state_for_timeout = state.clone();
            let reading_stack_for_timeout = reading_stack.clone();
            let content_stack_for_timeout = content_stack.clone();
            let mailbox_for_timeout = mailbox.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(HTML_REVEAL_TIMEOUT_MS), move || {
                let mut st = state_for_timeout.borrow_mut();
                let still_armed = st.pending_html_reveal && st.reveal_generation == generation;
                // Backstop for the same scenario the `load-changed` handler
                // guards against: `pending_html_reveal`/`reveal_generation`
                // disarmed by something other than a genuine navigation away
                // from this message, stranding the pane on "empty" with
                // nothing left to reveal it. If `rendered_message` still
                // names this exact message, revealing now is still correct.
                let stuck_on_empty =
                    st.rendered_message.as_ref() == Some(&(mailbox_for_timeout.clone(), uid)) && reading_stack_for_timeout.visible_child_name().as_deref() == Some("empty");
                if still_armed || stuck_on_empty {
                    st.pending_html_reveal = false;
                    drop(st);
                    reveal_message_page(&reading_stack_for_timeout, &content_stack_for_timeout, "html");
                }
            });
            return;
        }
    }
    if let Some(text) = &body.text_body {
        if let Some(scroller) = content_stack.child_by_name("text").and_downcast::<gtk::ScrolledWindow>() {
            if let Some(text_view) = scroller.child().and_downcast::<gtk::TextView>() {
                text_view.buffer().set_text(text);
                if animated && reading_stack.visible_child_name().as_deref() == Some("message") {
                    // Same-page re-render: route through "empty", wait out
                    // the fade-out, then reveal so both halves of the
                    // transition are visible.
                    let reading_stack_for_reveal = reading_stack.clone();
                    let content_stack_for_reveal = content_stack.clone();
                    let duration = reading_stack.transition_duration() as u64;
                    reading_stack.set_visible_child_name("empty");
                    glib::timeout_add_local_once(std::time::Duration::from_millis(duration), move || {
                        reveal_message_page(&reading_stack_for_reveal, &content_stack_for_reveal, "text");
                    });
                } else {
                    reveal_message_page(reading_stack, &content_stack, "text");
                }
                return;
            }
        }
    }
    reading_stack.set_visible_child_name("empty");
    state.borrow_mut().rendered_message = None;
}
